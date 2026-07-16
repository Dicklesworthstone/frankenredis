//! Same-binary proof for borrowing retained frames during online replica replay.
//!
//! The frozen reference is the literal current `drain_replica_stream` operation: clone the parsed
//! frame into the owned runtime entry point while retaining the original for diagnostics and
//! `REPLCONF GETACK` follow-up classification. The candidate borrows that retained frame.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

const VALUE_LEN: usize = 64 * 1024;
const PROFILE_REPEATS: usize = 200_000;
const STAT_REPEATS: usize = 2_000;
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
            Self::Candidate => &[
                "<fr_runtime::Runtime>::execute_frame_ref",
                "replica_frame_borrow::execute_online_replica_frame_borrowed_candidate",
            ],
            Self::Reference => &[
                "<fr_protocol::RespFrame as core::clone::Clone>::clone",
                "<alloc::vec::Vec<fr_protocol::RespFrame> as core::clone::Clone>::clone",
                "replica_frame_borrow::execute_online_replica_frame_owned_reference",
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

fn replay_set_frame() -> RespFrame {
    let mut value = Vec::with_capacity(VALUE_LEN);
    for index in 0..VALUE_LEN {
        value.push((index as u8).wrapping_mul(17).wrapping_add(31));
    }
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"SET".to_vec())),
        RespFrame::BulkString(Some(b"replay-key".to_vec())),
        RespFrame::BulkString(Some(value)),
    ]))
}

#[inline(never)]
fn execute_online_replica_frame_owned_reference(
    runtime: &mut Runtime,
    frame: &RespFrame,
    now_ms: u64,
) -> RespFrame {
    runtime.execute_frame(black_box(frame.clone()), black_box(now_ms))
}

#[inline(never)]
fn execute_online_replica_frame_borrowed_candidate(
    runtime: &mut Runtime,
    frame: &RespFrame,
    now_ms: u64,
) -> RespFrame {
    runtime.execute_frame_ref(black_box(frame), black_box(now_ms))
}

fn execute(runtime: &mut Runtime, frame: &RespFrame, now_ms: u64, arm: Arm) -> RespFrame {
    match arm {
        Arm::Candidate => execute_online_replica_frame_borrowed_candidate(runtime, frame, now_ms),
        Arm::Reference => execute_online_replica_frame_owned_reference(runtime, frame, now_ms),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut runtime = Runtime::default_strict();
    let frame = replay_set_frame();
    let retained = frame.clone();
    let mut checksum = 0_u64;
    for tick in 0..repeats {
        runtime.server.applying_master_stream = true;
        let response = execute(
            black_box(&mut runtime),
            black_box(&frame),
            black_box(tick as u64 + 1),
            arm,
        );
        runtime.server.applying_master_stream = false;
        checksum = checksum.wrapping_add(match black_box(&response) {
            RespFrame::SimpleString(value) => value.len() as u64,
            unexpected => panic!("unexpected replay response: {unexpected:?}"),
        });
        black_box(response);
        black_box(&frame);
    }
    assert_eq!(frame, retained, "retained replay frame changed");
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
        "fr_runtime_replica_frame_{}_{}_{}.data",
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
            "record",
            "-q",
            "-F",
            "997",
            "-e",
            "instructions:u",
            "-g",
            "-o",
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
    println!("TRIGGER command=SET value_bytes={VALUE_LEN} retained_frame=true");
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
        return Err(format!(
            "null median exposes harness bias: {null_median:.9}"
        ));
    }
    if effect_median <= null_p95 || effect_median <= 1.01 {
        return Err(format!(
            "candidate failed keep gate: effect={effect_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    Ok(())
}

fn correctness_sequence(arm: Arm) -> (Vec<RespFrame>, RespFrame) {
    let mut runtime = Runtime::default_strict();
    let large_set = replay_set_frame();
    let binary_set = command(&[b"SET", b"binary-key", b"\0\xff\r\nvalue"]);
    let frames = [
        command(&[b"SET", b"replay-key", b"first"]),
        command(&[b"GET", b"replay-key"]),
        command(&[b"REPLCONF", b"GETACK", b"*"]),
        binary_set,
        large_set,
        RespFrame::SimpleString("not-a-command".to_owned()),
    ];
    let mut replies = Vec::with_capacity(frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let retained = frame.clone();
        runtime.server.applying_master_stream = true;
        replies.push(execute(&mut runtime, frame, index as u64 + 1, arm));
        runtime.server.applying_master_stream = false;
        assert_eq!(*frame, retained, "frame {index} was not retained exactly");
    }
    let stored = runtime.execute_frame(command(&[b"GET", b"replay-key"]), 100);
    (replies, stored)
}

fn correctness_gate() {
    let candidate = correctness_sequence(Arm::Candidate);
    let reference = correctness_sequence(Arm::Reference);
    assert_eq!(candidate, reference);
    println!(
        "CORRECTNESS_GATE replies_state_and_retained_frames=identical cases={} value_bytes={VALUE_LEN}",
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
    if env::args().nth(1).as_deref() == Some("--profile-reference") {
        return run_profile(&executable, &[Arm::Reference])
            .map_err(|error| format!("PROFILE INVALID: {error}"));
    }
    correctness_gate();
    run_profile(&executable, &[Arm::Reference, Arm::Candidate])
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
