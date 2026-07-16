//! Same-binary instructions:u A/A+A/B for eliminating the per-command `SimpleString("OK")`
//! reply allocation on the borrowed RENAME success path.
//!
//! Like HMSET, RENAME replies `+OK` on success but an error frame ("no such key") when the source
//! is absent. [`Runtime::execute_plain_rename_borrowed`] now returns `Option<Option<RespFrame>>`:
//! `Some(None)` on success (the caller emits the constant `+OK\r\n` directly via `FastOkReply`, no
//! reply frame allocated) and `Some(Some(err))` on failure (routed through the ordinary `FastReply`
//! path). The frozen reference reconstructs the pre-change `SimpleString("OK")` frame on success and
//! encodes it; the candidate emits `+OK\r\n` directly. A `SimpleString("OK")` encodes to `+OK\r\n`
//! under RESP2 and RESP3, so the two arms are byte-identical on the wire and leave identical state.
//!
//! RENAME consumes its source key, so the A/B loop ping-pongs one value between two keys — every
//! iteration is a successful rename of an existing source to an absent target. The correctness gate
//! validates the borrowed fast path against the generic `execute_frame` RENAME for BOTH the +OK
//! success and the "no such key" error, proving the `Option<Option<RespFrame>>` refactor is faithful.

use std::{env, hint::black_box, path::Path, process::Command};

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

const KEY_A: &[u8] = b"rename-key-a";
const KEY_B: &[u8] = b"rename-key-b";
const VALUE: &[u8] = b"benchmark-value-";
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

/// Baseline: identical borrowed RENAME; on success reconstruct the `SimpleString("OK")` reply frame
/// and encode it into `out`, exactly like the pre-change consumer.
#[inline(never)]
fn rename_reply_owned_reference(
    runtime: &mut Runtime,
    from: &[u8],
    to: &[u8],
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    match runtime.execute_plain_rename_borrowed(black_box(from), black_box(to), black_box(now_ms)) {
        Some(None) => {
            let response = RespFrame::SimpleString("OK".to_string());
            response.encode_into(out);
            black_box(&response);
        }
        Some(Some(response)) => response.encode_into(out),
        None => panic!("RENAME fast path must engage for the benchmark keys"),
    }
}

/// Candidate: identical borrowed RENAME; emit the constant `+OK\r\n` directly on success
/// (`Some(None)`), or encode the error frame on failure.
#[inline(never)]
fn rename_reply_ok_candidate(
    runtime: &mut Runtime,
    from: &[u8],
    to: &[u8],
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    match runtime.execute_plain_rename_borrowed(black_box(from), black_box(to), black_box(now_ms)) {
        Some(None) => out.extend_from_slice(b"+OK\r\n"),
        Some(Some(response)) => response.encode_into(out),
        None => panic!("RENAME fast path must engage for the benchmark keys"),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut runtime = Runtime::default_strict();
    // Seed the source key; RENAME then ping-pongs the value between two keys so every iteration is
    // a successful rename of an existing source to an absent target.
    runtime.execute_frame(command(&[b"SET", KEY_A, VALUE]), 0);
    let mut out = Vec::with_capacity(16);
    let mut checksum = 0_u64;
    let mut forward = true;
    for tick in 0..repeats {
        out.clear();
        let (from, to): (&[u8], &[u8]) = if forward { (KEY_A, KEY_B) } else { (KEY_B, KEY_A) };
        match arm {
            Arm::Reference => {
                rename_reply_owned_reference(
                    black_box(&mut runtime),
                    from,
                    to,
                    black_box(tick as u64 + 1),
                    black_box(&mut out),
                );
            }
            Arm::Candidate => {
                rename_reply_ok_candidate(
                    black_box(&mut runtime),
                    from,
                    to,
                    black_box(tick as u64 + 1),
                    black_box(&mut out),
                );
            }
        }
        assert_eq!(out.as_slice(), b"+OK\r\n", "success reply must be +OK");
        checksum = checksum.wrapping_add(out.len() as u64);
        forward = !forward;
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

/// Reply bytes for one RENAME via the borrowed fast path (candidate rendering: +OK on success).
fn borrowed_rename_bytes(runtime: &mut Runtime, from: &[u8], to: &[u8], now_ms: u64) -> Vec<u8> {
    let mut out = Vec::new();
    rename_reply_ok_candidate(runtime, from, to, now_ms, &mut out);
    out
}

/// Reply bytes for one RENAME via the generic `execute_frame` dispatch (the authoritative oracle).
fn generic_rename_bytes(runtime: &mut Runtime, from: &[u8], to: &[u8], now_ms: u64) -> Vec<u8> {
    let mut out = Vec::new();
    runtime
        .execute_frame(command(&[b"RENAME", from, to]), now_ms)
        .encode_into(&mut out);
    out
}

fn correctness_gate() {
    let absent: &[u8] = b"rename-absent-src";
    let target: &[u8] = b"rename-target";

    // Borrowed fast path: success then "no such key".
    let mut rb = Runtime::default_strict();
    rb.execute_frame(command(&[b"SET", KEY_A, VALUE]), 0);
    let ok_b = borrowed_rename_bytes(&mut rb, KEY_A, KEY_B, 1);
    let get_new_b = rb.execute_frame(command(&[b"GET", KEY_B]), 2);
    let get_old_b = rb.execute_frame(command(&[b"GET", KEY_A]), 2);
    let err_b = borrowed_rename_bytes(&mut rb, absent, target, 3);

    // Generic dispatch oracle: identical sequence.
    let mut rg = Runtime::default_strict();
    rg.execute_frame(command(&[b"SET", KEY_A, VALUE]), 0);
    let ok_g = generic_rename_bytes(&mut rg, KEY_A, KEY_B, 1);
    let get_new_g = rg.execute_frame(command(&[b"GET", KEY_B]), 2);
    let get_old_g = rg.execute_frame(command(&[b"GET", KEY_A]), 2);
    let err_g = generic_rename_bytes(&mut rg, absent, target, 3);

    assert_eq!(ok_b, ok_g, "borrowed vs generic success reply diverged");
    assert_eq!(get_new_b, get_new_g, "renamed-to value diverged");
    assert_eq!(get_old_b, get_old_g, "renamed-from (now absent) diverged");
    assert_eq!(err_b, err_g, "borrowed vs generic no-such-key error diverged");
    assert_eq!(ok_b.as_slice(), b"+OK\r\n", "RENAME success must reply +OK");
    assert!(
        err_b.starts_with(b"-"),
        "RENAME on an absent source must reply an error, got {:?}",
        String::from_utf8_lossy(&err_b)
    );
    println!(
        "CORRECTNESS_GATE borrowed_matches_generic=success+no_such_key reply_ok={:?} reply_err={:?}",
        String::from_utf8_lossy(&ok_b),
        String::from_utf8_lossy(err_b.split(|&b| b == b'\r').next().unwrap_or(&err_b)),
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
    println!("TRIGGER command=RENAME value_bytes={}", VALUE.len());
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
