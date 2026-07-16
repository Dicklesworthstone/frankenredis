//! Same-binary instructions:u A/A+A/B for eliminating the per-write `SimpleString("OK")` reply
//! allocation on the borrowed `SET key value XX` success path (update-if-exists).
//!
//! SET XX replies `+OK` when it overwrites (key existed) and nil (`$-1`) when the key is absent.
//! [`Runtime::execute_plain_set_xx_borrowed`] now returns `Option<Option<RespFrame>>`: `Some(None)`
//! on a successful overwrite (the caller emits the constant `+OK\r\n` directly via `FastOkReply`, no
//! reply frame allocated) and `Some(Some(nil))` when the key was absent (the `$-1` reply — which
//! itself allocates nothing — is routed through the ordinary `FastReply` path). The frozen reference
//! reconstructs the pre-change `SimpleString("OK")` frame on success and encodes it; the candidate
//! emits `+OK\r\n` directly. Byte-identical on the wire (RESP2/RESP3) with identical store state.
//!
//! Unlike SET NX, SET XX leaves the key present, so a single seeded key makes every iteration a
//! successful overwrite with NO per-iteration reset — the A/B is undiluted and reflects the family's
//! true per-op benefit. The correctness gate validates the borrowed fast path against the generic
//! `execute_frame` SET XX for BOTH the +OK overwrite and the `$-1` nil (key absent) outcome.

use std::{env, hint::black_box, path::Path, process::Command};

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

const KEY: &[u8] = b"set-xx-key";
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

/// Baseline: identical borrowed SET XX; on a successful overwrite reconstruct the
/// `SimpleString("OK")` reply frame and encode it into `out`, exactly like the pre-change consumer.
#[inline(never)]
fn set_xx_reply_owned_reference(runtime: &mut Runtime, now_ms: u64, out: &mut Vec<u8>) {
    match runtime.execute_plain_set_xx_borrowed(black_box(KEY), black_box(VALUE), black_box(now_ms)) {
        Some(None) => {
            let response = RespFrame::SimpleString("OK".to_string());
            response.encode_into(out);
            black_box(&response);
        }
        Some(Some(response)) => response.encode_into(out),
        None => panic!("SET XX fast path must engage for the benchmark key"),
    }
}

/// Candidate: identical borrowed SET XX; emit the constant `+OK\r\n` directly on a successful
/// overwrite (`Some(None)`), or encode the nil frame when the key was absent.
#[inline(never)]
fn set_xx_reply_ok_candidate(runtime: &mut Runtime, now_ms: u64, out: &mut Vec<u8>) {
    match runtime.execute_plain_set_xx_borrowed(black_box(KEY), black_box(VALUE), black_box(now_ms)) {
        Some(None) => out.extend_from_slice(b"+OK\r\n"),
        Some(Some(response)) => response.encode_into(out),
        None => panic!("SET XX fast path must engage for the benchmark key"),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut runtime = Runtime::default_strict();
    // Seed the key once; SET XX then overwrites it every iteration (key stays present → success →
    // +OK) with no per-iteration reset, so the A/B is undiluted.
    runtime.execute_frame(command(&[b"SET", KEY, VALUE]), 0);
    let mut out = Vec::with_capacity(16);
    let mut checksum = 0_u64;
    for tick in 0..repeats {
        out.clear();
        match arm {
            Arm::Reference => {
                set_xx_reply_owned_reference(
                    black_box(&mut runtime),
                    black_box(tick as u64 + 1),
                    black_box(&mut out),
                );
            }
            Arm::Candidate => {
                set_xx_reply_ok_candidate(
                    black_box(&mut runtime),
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

/// One SET XX via the borrowed fast path (candidate rendering: +OK on overwrite, nil when absent).
fn borrowed_set_xx_bytes(runtime: &mut Runtime, now_ms: u64) -> Vec<u8> {
    let mut out = Vec::new();
    set_xx_reply_ok_candidate(runtime, now_ms, &mut out);
    out
}

/// One SET XX via the generic `execute_frame` dispatch (the authoritative oracle).
fn generic_set_xx_bytes(runtime: &mut Runtime, now_ms: u64) -> Vec<u8> {
    let mut out = Vec::new();
    runtime
        .execute_frame(command(&[b"SET", KEY, VALUE, b"XX"]), now_ms)
        .encode_into(&mut out);
    out
}

fn correctness_gate() {
    // Borrowed fast path: existing key → +OK, then a fresh (absent-key) store → nil.
    let mut rb = Runtime::default_strict();
    rb.execute_frame(command(&[b"SET", KEY, VALUE]), 0);
    let ok_b = borrowed_set_xx_bytes(&mut rb, 1);
    let get_b = rb.execute_frame(command(&[b"GET", KEY]), 2);
    let mut rb_absent = Runtime::default_strict();
    let nil_b = borrowed_set_xx_bytes(&mut rb_absent, 3);

    // Generic dispatch oracle: identical sequences.
    let mut rg = Runtime::default_strict();
    rg.execute_frame(command(&[b"SET", KEY, VALUE]), 0);
    let ok_g = generic_set_xx_bytes(&mut rg, 1);
    let get_g = rg.execute_frame(command(&[b"GET", KEY]), 2);
    let mut rg_absent = Runtime::default_strict();
    let nil_g = generic_set_xx_bytes(&mut rg_absent, 3);

    assert_eq!(ok_b, ok_g, "borrowed vs generic overwrite reply diverged");
    assert_eq!(get_b, get_g, "stored value diverged");
    assert_eq!(nil_b, nil_g, "borrowed vs generic nil (key absent) reply diverged");
    assert_eq!(ok_b.as_slice(), b"+OK\r\n", "SET XX overwrite must reply +OK");
    assert_eq!(nil_b.as_slice(), b"$-1\r\n", "SET XX on an absent key must reply nil");
    println!(
        "CORRECTNESS_GATE borrowed_matches_generic=overwrite+absent reply_ok={:?} reply_nil={:?}",
        String::from_utf8_lossy(&ok_b),
        String::from_utf8_lossy(&nil_b),
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
        "TRIGGER command=SET_XX key_bytes={} value_bytes={}",
        KEY.len(),
        VALUE.len()
    );
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
