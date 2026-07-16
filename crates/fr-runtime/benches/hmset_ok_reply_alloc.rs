//! Same-binary instructions:u A/A+A/B for eliminating the per-command `SimpleString("OK")`
//! reply allocation on the borrowed HMSET success path.
//!
//! Unlike the unconditional SET/MSET +OK paths, HMSET can reply either `+OK` (success) or an error
//! frame (e.g. WRONGTYPE). The new [`Runtime::execute_plain_hmset_borrowed_ok`] twin factors this:
//! `Some(None)` on success (the caller emits the constant `+OK\r\n` directly via `FastOkReply`, no
//! reply frame allocated) and `Some(Some(err))` on failure (routed through the ordinary `FastReply`
//! path, unchanged). The frozen reference reproduces the pre-change path via
//! [`Runtime::execute_plain_hmset_borrowed`] (which allocates the `SimpleString("OK")` frame on
//! success) and encodes it. A `SimpleString("OK")` encodes to `+OK\r\n` under RESP2 and RESP3, so
//! the two arms are byte-identical on the wire and leave identical store state.
//!
//! The A/B measures the success path (the perf-relevant one); the correctness gate additionally
//! exercises the WRONGTYPE error path to prove the success/error split routes bytes identically.

use std::{env, hint::black_box, path::Path, process::Command};

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

const KEY: &[u8] = b"hmset-hash-key";
// Flat [field, value, field, value, …] as HMSET's borrowed executor consumes them.
const PAIRS: [&[u8]; 6] = [
    b"field-0",
    b"benchmark-value-0",
    b"field-1",
    b"benchmark-value-1",
    b"field-2",
    b"benchmark-value-2",
];
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

/// Baseline: identical borrowed HMSET write via the frame-returning wrapper, then encode whatever
/// frame it produced (a `SimpleString("OK")` on success), exactly like the pre-change consumer.
#[inline(never)]
fn hmset_reply_owned_reference(runtime: &mut Runtime, key: &[u8], now_ms: u64, out: &mut Vec<u8>) {
    let response = runtime
        .execute_plain_hmset_borrowed(black_box(key), black_box(&PAIRS), black_box(now_ms))
        .expect("HMSET fast path must accept the benchmark pairs");
    response.encode_into(out);
    black_box(&response);
}

/// Candidate: identical borrowed HMSET write via the non-allocating twin; emit the constant
/// `+OK\r\n` directly on success (`Some(None)`), or encode the error frame on failure.
#[inline(never)]
fn hmset_reply_ok_candidate(runtime: &mut Runtime, key: &[u8], now_ms: u64, out: &mut Vec<u8>) {
    match runtime.execute_plain_hmset_borrowed_ok(black_box(key), black_box(&PAIRS), black_box(now_ms)) {
        Some(None) => out.extend_from_slice(b"+OK\r\n"),
        Some(Some(response)) => response.encode_into(out),
        None => panic!("HMSET fast path must accept the benchmark pairs"),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut runtime = Runtime::default_strict();
    let mut out = Vec::with_capacity(16);
    let mut checksum = 0_u64;
    for tick in 0..repeats {
        out.clear();
        match arm {
            Arm::Reference => {
                hmset_reply_owned_reference(
                    black_box(&mut runtime),
                    black_box(KEY),
                    black_box(tick as u64 + 1),
                    black_box(&mut out),
                );
            }
            Arm::Candidate => {
                hmset_reply_ok_candidate(
                    black_box(&mut runtime),
                    black_box(KEY),
                    black_box(tick as u64 + 1),
                    black_box(&mut out),
                );
            }
        }
        assert_eq!(out.as_slice(), b"+OK\r\n", "success reply must be +OK");
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

/// Reply bytes for one HMSET via the given arm (success or error), without asserting +OK — used by
/// the correctness gate to compare the success AND WRONGTYPE-error paths byte-for-byte.
fn hmset_reply_bytes(arm: Arm, runtime: &mut Runtime, key: &[u8], now_ms: u64) -> Vec<u8> {
    let mut out = Vec::new();
    match arm {
        Arm::Reference => {
            let response = runtime
                .execute_plain_hmset_borrowed(key, &PAIRS, now_ms)
                .expect("HMSET borrowed fast path must engage");
            response.encode_into(&mut out);
        }
        Arm::Candidate => {
            match runtime.execute_plain_hmset_borrowed_ok(key, &PAIRS, now_ms) {
                Some(None) => out.extend_from_slice(b"+OK\r\n"),
                Some(Some(response)) => response.encode_into(&mut out),
                None => panic!("HMSET borrowed fast path must engage"),
            }
        }
    }
    out
}

fn correctness_sequence(arm: Arm) -> (Vec<Vec<u8>>, Vec<RespFrame>) {
    let mut runtime = Runtime::default_strict();
    let mut replies = Vec::new();
    // Two successful HMSETs (create then overwrite) → +OK each.
    replies.push(hmset_reply_bytes(arm, &mut runtime, KEY, 1));
    replies.push(hmset_reply_bytes(arm, &mut runtime, KEY, 2));
    // WRONGTYPE: HMSET onto a key already holding a plain string must produce the error frame,
    // and the success/error split must route those bytes identically to the reference.
    let string_key: &[u8] = b"hmset-string-key";
    runtime.execute_frame(command(&[b"SET", string_key, b"iamastring"]), 3);
    replies.push(hmset_reply_bytes(arm, &mut runtime, string_key, 4));
    // Inspect resulting state over the public surface: the hash fields and the untouched string.
    let stored = vec![
        runtime.execute_frame(command(&[b"HGETALL", KEY]), 100),
        runtime.execute_frame(command(&[b"GET", string_key]), 100),
    ];
    (replies, stored)
}

fn correctness_gate() {
    let candidate = correctness_sequence(Arm::Candidate);
    let reference = correctness_sequence(Arm::Reference);
    assert_eq!(
        candidate, reference,
        "candidate replies/store state diverged from reference"
    );
    assert_eq!(candidate.0[0].as_slice(), b"+OK\r\n", "HMSET create must reply +OK");
    assert_eq!(candidate.0[1].as_slice(), b"+OK\r\n", "HMSET overwrite must reply +OK");
    assert!(
        candidate.0[2].starts_with(b"-WRONGTYPE"),
        "HMSET on a string key must reply a WRONGTYPE error, got {:?}",
        String::from_utf8_lossy(&candidate.0[2])
    );
    println!(
        "CORRECTNESS_GATE success_and_wrongtype_replies_and_state=identical cases={}",
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
    println!("TRIGGER command=HMSET fields={}", PAIRS.len() / 2);
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
