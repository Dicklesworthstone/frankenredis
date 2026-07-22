//! Same-binary proof for the small-length const-size bulk header path in
//! `encode_bulk_string_slice` (frankenredis-vlis9).
//!
//! Candidate emits `$<len>\r\n` for `len < 100` as one const-length `extend_from_slice`
//! (no per-element `reserve`, no `ilog10`, no stack-buffer build); reference is the exact
//! prior shape (`reserve` + fused `push_len_header`). Bodies are written identically in
//! both arms. The workload is the per-element collection-reply loop (HGETALL / LRANGE /
//! SMEMBERS shape) where the live HGETALL(10k) profile put `push_len_header` at 22.2% and
//! `encode_bulk_string_slice` at 26.7% self.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::bench_encode_bulk_string_slice_small;

const PROFILE_REPEATS: usize = 400_000;
const PROFILE_TRIALS: usize = 3;
const STAT_REPEATS: usize = 250_000;
const STAT_ROUNDS: usize = 24;

const FILLER: &[u8] = &[b'v'; 128];

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

// Element bodies per simulated reply: dominated by the HGETALL field/value widths
// (one- and two-digit lengths), with a two-digit cluster and one >=100 fallback element
// so the fallback stays exercised (and its neutrality is part of the measurement).
const ELEMENT_LENS: [usize; 16] = [7, 9, 8, 11, 9, 10, 8, 9, 24, 33, 47, 63, 9, 8, 11, 104];
const ELEMENTS_PER_REPLY: usize = 20;

fn run_loop(arm: Arm, repeats: usize) {
    // One reusable buffer whose capacity stabilizes after the first reply, so the timed
    // delta is per-element header emission, never allocation.
    let mut out: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut checksum: u64 = 0;
    for _ in 0..repeats {
        out.clear();
        for slot in 0..ELEMENTS_PER_REPLY {
            let len = ELEMENT_LENS[slot % ELEMENT_LENS.len()];
            let body = black_box(&FILLER[..len]);
            match arm {
                Arm::Candidate => {
                    bench_encode_bulk_string_slice_small::<true>(Some(body), false, &mut out);
                }
                Arm::Reference => {
                    bench_encode_bulk_string_slice_small::<false>(Some(body), false, &mut out);
                }
            }
        }
        let last = out.len() - 1;
        checksum = checksum
            .wrapping_add(out.len() as u64)
            .wrapping_add(out[0] as u64)
            .wrapping_add(out[last] as u64);
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

fn profile_trial(executable: &Path, trial: usize) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_encode_bulk_small_{}_{}_{}.data",
        process::id(),
        trial,
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
        .args([
            "--child",
            Arm::Reference.name(),
            &PROFILE_REPEATS.to_string(),
        ])
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
            "--percent-limit",
            "0.1",
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
    println!("PROFILE_TABLE_BEGIN trial={trial}\n{stdout}\nPROFILE_TABLE_END trial={trial}");
    let line = stdout
        .lines()
        .find(|line| line.contains("fr_protocol::bench_encode_bulk_string_slice_small"))
        .ok_or("profile has no reference bulk-slice frame; workload INVALID")?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err("reference encoder has zero self-time; workload INVALID".into());
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path) -> Result<(), String> {
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("WORKER_ID {hostname}");
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    for arm in [Arm::Reference, Arm::Candidate] {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
    }
    let mut samples = Vec::with_capacity(PROFILE_TRIALS);
    for trial in 1..=PROFILE_TRIALS {
        let self_pct = profile_trial(executable, trial)?;
        println!("PROFILE_SELF arm=reference trial={trial} self_pct={self_pct:.4}");
        samples.push(self_pct);
    }
    let self_cv_pct = cv(&samples);
    let median_self_pct = median(&mut samples);
    println!(
        "PROFILE_SELF_SUMMARY arm=reference trials={PROFILE_TRIALS} median_self_pct={median_self_pct:.4} self_cv_pct={self_cv_pct:.4} samples={samples:?}"
    );
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

fn correctness_gate() {
    let payload: Vec<u8> = (0..=1100_usize).map(|i| (i % 251) as u8).collect();
    let mut cases = 0_usize;
    for resp3 in [false, true] {
        for len in (0..=300).chain([511, 512, 999, 1000, 1100]) {
            let mut fast = b"X".to_vec();
            let mut slow = b"X".to_vec();
            bench_encode_bulk_string_slice_small::<true>(Some(&payload[..len]), resp3, &mut fast);
            bench_encode_bulk_string_slice_small::<false>(Some(&payload[..len]), resp3, &mut slow);
            assert_eq!(fast, slow, "bulk slice differs len={len} resp3={resp3}");
            cases += 1;
        }
        let mut fast = Vec::new();
        let mut slow = Vec::new();
        bench_encode_bulk_string_slice_small::<true>(None, resp3, &mut fast);
        bench_encode_bulk_string_slice_small::<false>(None, resp3, &mut slow);
        assert_eq!(fast, slow, "nil bulk differs resp3={resp3}");
        cases += 1;
    }
    let mut known = Vec::new();
    bench_encode_bulk_string_slice_small::<true>(Some(b"hi"), false, &mut known);
    assert_eq!(known, b"$2\r\nhi\r\n");
    println!("CORRECTNESS_GATE result=identical cases={cases} known_literal=covered");
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
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
    }
    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let null_p05 = percentile(&nulls, 0.05);
    let null_p95 = percentile(&nulls, 0.95);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} speedup_cv_pct={effect_cv_pct:.6}"
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

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = child_args()? {
        run_loop(arm, repeats);
        return Ok(());
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    correctness_gate();
    run_profile(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
