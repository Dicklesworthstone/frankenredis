//! Same-binary instructions:u A/A+A/B for eliminating the per-write `SimpleString("OK")`
//! reply allocation on the borrowed `SET key value EXAT|PXAT <ts>` fast path.
//!
//! The frozen reference reproduces the pre-change `FastReply` path: run the borrowed
//! absexpire write via [`Runtime::execute_plain_set_absexpire_borrowed`] (which allocates a
//! `SimpleString("OK")` reply frame) and encode it into the connection buffer. The candidate
//! runs the identical write via [`Runtime::execute_plain_set_absexpire_borrowed_ok`] and emits
//! the constant `+OK\r\n` straight into the buffer (the `FastOkReply` path), so no reply frame
//! is allocated. A `SimpleString("OK")` encodes to `+OK\r\n` under both RESP2 and RESP3, so the
//! two arms are byte-identical on the wire and leave identical store state (value + absolute TTL).
//!
//! Representative of the SET-modifier reply-alloc closeout: the EX/PX (relexpire) sibling shipped
//! at 1.0678x (set_relexpire_ok_reply_alloc.rs); EXAT/PXAT (here) and KEEPTTL share the identical
//! mechanism and are covered for correctness by the fr-runtime conformance suite.

use std::{env, hint::black_box, path::Path, process::Command};

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

const KEY: &[u8] = b"set-absexpire-key";
const VALUE: &[u8] = b"benchmark-value-";
// PXAT far-future absolute timestamp (ms): the fast path takes the millisecond branch directly
// and the key never expires across the bench's small monotonically-increasing now_ms.
const TIME_ARG: &[u8] = b"10000000000";
const IS_SECONDS: bool = false;
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
}

fn command(parts: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some(part.to_vec())))
            .collect(),
    ))
}

/// Baseline: identical borrowed absexpire write, then allocate the `SimpleString("OK")` reply
/// frame and encode it into `out`, exactly like the pre-change `FastReply` consumer.
#[inline(never)]
fn absexpire_reply_owned_reference(
    runtime: &mut Runtime,
    key: &[u8],
    value: &[u8],
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    let response = runtime
        .execute_plain_set_absexpire_borrowed(
            black_box(IS_SECONDS),
            black_box(key),
            black_box(value),
            black_box(TIME_ARG),
            black_box(now_ms),
        )
        .expect("absexpire SET fast path must accept the benchmark key");
    response.encode_into(out);
    black_box(&response);
}

/// Candidate: identical borrowed absexpire write via the non-allocating twin, then emit the
/// constant `+OK\r\n` directly, exactly like the `FastOkReply` consumer.
#[inline(never)]
fn absexpire_reply_ok_candidate(
    runtime: &mut Runtime,
    key: &[u8],
    value: &[u8],
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    runtime
        .execute_plain_set_absexpire_borrowed_ok(
            black_box(IS_SECONDS),
            black_box(key),
            black_box(value),
            black_box(TIME_ARG),
            black_box(now_ms),
        )
        .expect("absexpire SET fast path must accept the benchmark key");
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
                absexpire_reply_owned_reference(
                    black_box(&mut runtime),
                    black_box(KEY),
                    black_box(VALUE),
                    black_box(tick as u64 + 1),
                    black_box(&mut out),
                );
            }
            Arm::Candidate => {
                absexpire_reply_ok_candidate(
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

fn worker_id() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
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

fn correctness_sequence(arm: Arm) -> (Vec<Vec<u8>>, Vec<RespFrame>, Vec<RespFrame>) {
    let mut runtime = Runtime::default_strict();
    let mut out = Vec::new();
    let cases: [(&[u8], &[u8]); 5] = [
        (b"set-absexpire-key", b"first"),
        (b"set-absexpire-key", b"overwrite"),
        (b"binary-key", b"\0\xff\r\nvalue"),
        (b"empty-key", b""),
        (b"set-absexpire-key", b"benchmark-value-"),
    ];
    let mut replies = Vec::with_capacity(cases.len());
    for (index, (key, value)) in cases.iter().enumerate() {
        out.clear();
        let now = index as u64 + 1;
        match arm {
            Arm::Reference => absexpire_reply_owned_reference(&mut runtime, key, value, now, &mut out),
            Arm::Candidate => {
                absexpire_reply_ok_candidate(&mut runtime, key, value, now, &mut out);
            }
        }
        replies.push(out.clone());
    }
    let keys: [&[u8]; 3] = [b"set-absexpire-key", b"binary-key", b"empty-key"];
    let stored = keys
        .into_iter()
        .map(|key| runtime.execute_frame(command(&[b"GET", key]), 100))
        .collect();
    // Absolute TTL must be preserved identically: query PTTL over the public dispatch surface so
    // both arms are compared on the exact same remaining ms.
    let ttls = keys
        .into_iter()
        .map(|key| runtime.execute_frame(command(&[b"PTTL", key]), 100))
        .collect();
    (replies, stored, ttls)
}

fn correctness_gate() {
    let candidate = correctness_sequence(Arm::Candidate);
    let reference = correctness_sequence(Arm::Reference);
    assert_eq!(
        candidate, reference,
        "candidate replies/store state/TTL diverged from reference"
    );
    for reply in &candidate.0 {
        assert_eq!(reply.as_slice(), b"+OK\r\n", "each absexpire SET reply must be +OK");
    }
    println!(
        "CORRECTNESS_GATE replies_store_state_and_ttl=identical cases={}",
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
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(&executable)?);
    println!(
        "TRIGGER command=SET_PXAT key_bytes={} value_bytes={} pxat_ms={}",
        KEY.len(),
        VALUE.len(),
        String::from_utf8_lossy(TIME_ARG),
    );
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
