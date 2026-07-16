//! Same-binary instructions:u A/A+A/B for eliminating the per-SET `SimpleString("OK")`
//! reply allocation on the borrowed plain-SET fast path.
//!
//! The frozen reference reproduces the pre-change `FastReply` path: run the borrowed
//! plain-SET write, then allocate a `SimpleString("OK")` reply frame and encode it into
//! the connection buffer. The candidate runs the identical write via
//! `execute_plain_set_borrowed_ok` and emits the constant `+OK\r\n` straight into the
//! buffer (the new `FastOkReply` path), so no reply frame is allocated. A
//! `SimpleString("OK")` encodes to `+OK\r\n` under both RESP2 and RESP3, so the two arms
//! are byte-identical on the wire and leave identical store state.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

const KEY: &[u8] = b"set-reply-key";
const VALUE: &[u8] = b"benchmark-value-";
const PROFILE_REPEATS: usize = 400_000;
const STAT_REPEATS: usize = 6_000;
const STAT_ROUNDS: usize = 9;

#[derive(Clone, Copy)]
enum Arm {
    Candidate,
    Reference,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Reference => "reference",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "reference" => Ok(Self::Reference),
            _ => Err(format!("unknown arm {value:?}")),
        }
    }

    const fn profile_symbols(self) -> &'static [&'static str] {
        match self {
            // The candidate's distinguishing work (a 5-byte `+OK` write) is below
            // perf's sampling floor precisely because it is so cheap — that near-zero
            // self-time IS the win. Provenance instead pins the shared SET write, which
            // proves the candidate arm executed the real store path (not a no-op). The
            // removable alloc/encode work is attributed on the reference arm below.
            Self::Candidate => &[
                "<fr_store::Store>::set_plain_borrowed",
                "set_ok_reply_alloc::set_reply_ok_candidate",
            ],
            Self::Reference => &[
                "set_ok_reply_alloc::set_reply_owned_reference",
                "<fr_store::Store>::set_plain_borrowed",
            ],
        }
    }
}

fn command(parts: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some(part.to_vec())))
            .collect(),
    ))
}

/// Baseline: identical borrowed write, then allocate the `SimpleString("OK")` reply and
/// encode it into `out`, exactly like the pre-change `FastReply` consumer.
#[inline(never)]
fn set_reply_owned_reference(
    runtime: &mut Runtime,
    key: &[u8],
    value: &[u8],
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    let response = runtime
        .execute_plain_set_borrowed(black_box(key), black_box(value), black_box(now_ms))
        .expect("plain SET fast path must accept the benchmark key");
    response.encode_into(out);
    black_box(&response);
}

/// Candidate: identical borrowed write via the non-allocating twin, then emit the
/// constant `+OK\r\n` directly, exactly like the new `FastOkReply` consumer.
#[inline(never)]
fn set_reply_ok_candidate(
    runtime: &mut Runtime,
    key: &[u8],
    value: &[u8],
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    runtime
        .execute_plain_set_borrowed_ok(black_box(key), black_box(value), black_box(now_ms))
        .expect("plain SET fast path must accept the benchmark key");
    out.extend_from_slice(b"+OK\r\n");
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut runtime = Runtime::default_strict();
    let mut out = Vec::with_capacity(16);
    let mut checksum = 0_u64;
    for tick in 0..repeats {
        out.clear();
        match arm {
            Arm::Reference => {
                set_reply_owned_reference(
                    black_box(&mut runtime),
                    black_box(KEY),
                    black_box(VALUE),
                    black_box(tick as u64 + 1),
                    black_box(&mut out),
                );
            }
            Arm::Candidate => {
                set_reply_ok_candidate(
                    black_box(&mut runtime),
                    black_box(KEY),
                    black_box(VALUE),
                    black_box(tick as u64 + 1),
                    black_box(&mut out),
                );
            }
        }
        assert_eq!(out.as_slice(), b"+OK\r\n", "reply must be +OK");
        checksum = checksum.wrapping_add(out.len() as u64);
        black_box(&out);
    }
    black_box(checksum);
}

fn child_args() -> Result<Option<(Arm, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--child") {
        return Ok(None);
    }
    let arm = Arm::parse(args.get(2).ok_or("missing child arm")?)?;
    let repeats = args
        .get(3)
        .ok_or("missing child repeat count")?
        .parse()
        .map_err(|error| format!("invalid repeat count: {error}"))?;
    Ok(Some((arm, repeats)))
}

fn binary_sha256(executable: &Path) -> Result<String, String> {
    let output = Command::new("sha256sum")
        .arg(executable)
        .output()
        .map_err(|error| format!("could not launch sha256sum: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum emitted no digest".to_owned())
}

fn worker_id() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn profile_trial(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_runtime_set_ok_reply_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let recorded = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "record", "-q", "-F", "997", "-e", "instructions:u", "-g", "-o",
        ])
        .arg(&data)
        .arg("--")
        .arg(executable)
        .args(["--child", arm.name(), &PROFILE_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf record: {error}"))?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let report = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "report",
            "-i",
            data.to_str().ok_or("non-UTF-8 perf.data path")?,
            "--stdio",
            "--no-children",
            "-g",
            "none",
            "--percent-limit",
            "0.01",
        ])
        .output()
        .map_err(|error| format!("could not launch perf report: {error}"))?;
    if !report.status.success() {
        return Err(format!(
            "perf report failed: {}",
            String::from_utf8_lossy(&report.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&report.stdout);
    println!(
        "PROFILE_TABLE_BEGIN arm={}\n{stdout}\nPROFILE_TABLE_END arm={}",
        arm.name(),
        arm.name()
    );
    let lost_samples = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted Total Lost Samples; profile provenance INVALID")?
        .rsplit(':')
        .next()
        .ok_or("missing lost-sample count")?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost_samples != 0 {
        return Err(format!("profile lost {lost_samples} samples"));
    }
    let line = stdout
        .lines()
        .find(|line| {
            arm.profile_symbols()
                .iter()
                .any(|symbol| line.contains(symbol))
                && !line.contains("closure#")
        })
        .ok_or_else(|| {
            format!(
                "profile has no exact {} frame in {:?}; workload INVALID",
                arm.name(),
                arm.profile_symbols()
            )
        })?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err(format!("{} target has zero self-time", arm.name()));
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path, arms: &[Arm]) -> Result<(), String> {
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!("TRIGGER command=SET key_bytes={} value_bytes={}", KEY.len(), VALUE.len());
    for &arm in arms {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "10"])
            .status()
            .map_err(|error| format!("could not launch run warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} run warm-up failed", arm.name()));
        }
        let self_pct = profile_trial(executable, arm)?;
        println!("PROFILE_SELF arm={} self_pct={self_pct:.4}", arm.name());
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat failed: {stderr}"));
    }
    stderr
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split(',').collect();
            fields
                .iter()
                .any(|field| field.trim().contains("instructions"))
                .then(|| fields[0].trim())
        })
        .ok_or_else(|| format!("instructions:u missing: {stderr}"))?
        .parse()
        .map_err(|error| format!("invalid instruction count: {error}"))
}

fn cv(samples: &[f64]) -> f64 {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    100.0 * variance.sqrt() / mean
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| left.partial_cmp(right).expect("sample is not NaN"));
    samples[samples.len() / 2]
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    let mut candidate_counts = Vec::with_capacity(STAT_ROUNDS);
    let mut reference_counts = Vec::with_capacity(STAT_ROUNDS);
    for round in 0..STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                Arm::Reference
            } else {
                Arm::Candidate
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null = counts[0] as f64 / counts[1] as f64;
        let effect = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} null_ratio={null:.9} reference_over_candidate={effect:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        nulls.push(null);
        effects.push(effect);
        candidate_counts.push(counts[0] as f64);
        reference_counts.push(counts[2] as f64);
    }
    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let candidate_median = median(&mut candidate_counts);
    let reference_median = median(&mut reference_counts);
    let null_p05 = percentile(&nulls, 0.05);
    let null_p95 = percentile(&nulls, 0.95);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} speedup_cv_pct={effect_cv_pct:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!("null median exposes harness bias: {null_median:.9}"));
    }
    if effect_median <= null_p95 || effect_median <= 1.01 {
        return Err(format!(
            "candidate failed keep gate: effect={effect_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    Ok(())
}

fn correctness_sequence(arm: Arm) -> (Vec<Vec<u8>>, Vec<RespFrame>) {
    let mut runtime = Runtime::default_strict();
    let mut out = Vec::new();
    let cases: [(&[u8], &[u8]); 5] = [
        (b"set-reply-key", b"first"),
        (b"set-reply-key", b"overwrite"),
        (b"binary-key", b"\0\xff\r\nvalue"),
        (b"empty-key", b""),
        (b"set-reply-key", b"benchmark-value-"),
    ];
    let mut replies = Vec::with_capacity(cases.len());
    for (index, (key, value)) in cases.iter().enumerate() {
        out.clear();
        match arm {
            Arm::Reference => set_reply_owned_reference(
                &mut runtime,
                key,
                value,
                index as u64 + 1,
                &mut out,
            ),
            Arm::Candidate => {
                set_reply_ok_candidate(&mut runtime, key, value, index as u64 + 1, &mut out);
            }
        }
        replies.push(out.clone());
    }
    let stored = [b"set-reply-key".as_slice(), b"binary-key", b"empty-key"]
        .into_iter()
        .map(|key| runtime.execute_frame(command(&[b"GET", key]), 100))
        .collect();
    (replies, stored)
}

fn correctness_gate() {
    let candidate = correctness_sequence(Arm::Candidate);
    let reference = correctness_sequence(Arm::Reference);
    assert_eq!(
        candidate, reference,
        "candidate replies/store state diverged from reference"
    );
    for reply in &candidate.0 {
        assert_eq!(reply.as_slice(), b"+OK\r\n", "each SET reply must be +OK");
    }
    println!(
        "CORRECTNESS_GATE replies_and_store_state=identical cases={}",
        candidate.0.len()
    );
}

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = child_args()? {
        run_loop(arm, repeats);
        return Ok(());
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    correctness_gate();
    run_profile(&executable, &[Arm::Reference, Arm::Candidate])
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
