//! Same-binary A/B for the `SORT ... ALPHA` comparator.
//!
//! One parent invocation profiles the bench-only ORIG arm, then position-balances an
//! ORIG/ORIG A/A null with ORIG/CAND `perf stat instructions:u`. The keep gate uses a
//! bootstrap median CI and 2x null margin; CV is provenance only. This is deliberately a
//! custom `harness = false` main: separate Cargo invocations would let RCH choose different
//! workers and invalidate the ratio.

use std::{
    cmp::Ordering,
    env, fs,
    hint::black_box,
    path::{Path, PathBuf},
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use criterion::{BenchmarkId, Criterion};
use icu_collator::CollatorBorrowed;
use sha2::{Digest, Sha256};

const PROFILE_REPEATS: usize = 50;
const STAT_REPEATS: usize = 100;
const STAT_ROUNDS: usize = 12;
const STAT_LEN: usize = 32;
const CORPUS_COUNT: usize = 1_000;
const KEEP_GATE_RATIO: f64 = 0.99;
const BOOTSTRAP_RESAMPLES: usize = 20_000;

#[derive(Clone, Copy, Debug)]
enum Arm {
    Orig,
    Candidate,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Orig => "orig",
            Self::Candidate => "candidate",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "orig" => Ok(Self::Orig),
            "candidate" => Ok(Self::Candidate),
            _ => Err(format!("unknown child arm {value:?}")),
        }
    }
}

/// The semantically exact pre-short-circuit comparator, kept out of line and fed a
/// runtime-opaque `Option`. The result barriers preserve the historical eager validation;
/// without them LLVM legally erases the pure calls when `collator` is `None`.
#[inline(never)]
fn orig_sort_alpha_compare(
    collator: Option<&CollatorBorrowed<'_>>,
    left: &[u8],
    right: &[u8],
) -> Ordering {
    let collator = black_box(collator);
    let left_utf8 = black_box(std::str::from_utf8(left));
    let right_utf8 = black_box(std::str::from_utf8(right));
    match (collator, left_utf8, right_utf8) {
        (Some(collator), Ok(left), Ok(right)) if !left.contains('\0') && !right.contains('\0') => {
            collator.compare(left, right)
        }
        _ => left.cmp(right),
    }
}

#[inline(never)]
fn candidate_sort_alpha_compare(
    collator: Option<&CollatorBorrowed<'_>>,
    left: &[u8],
    right: &[u8],
) -> Ordering {
    fr_command::sort_alpha_compare(black_box(collator), black_box(left), black_box(right))
}

/// Elements shaped like a real `SORT ALPHA` payload: equal length and shared prefixes avoid
/// turning the comparison into an early length mismatch.
fn corpus(count: usize, len: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            let mut value = vec![b'e'; len];
            let tag = format!("{:08}", (i * 7919) % count);
            value[len - tag.len()..].copy_from_slice(tag.as_bytes());
            value
        })
        .collect()
}

fn run_loop(
    refs: &[&[u8]],
    repeats: usize,
    mut compare: impl FnMut(&[u8], &[u8]) -> Ordering,
) -> i64 {
    let mut accumulator = 0_i64;
    for _ in 0..repeats {
        // One barrier per pass prevents hoisting without diluting every comparison.
        let current = black_box(refs);
        for pair in current.windows(2) {
            let delta = match compare(pair[0], pair[1]) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            };
            accumulator = accumulator.wrapping_add(delta);
        }
        accumulator = black_box(accumulator);
    }
    accumulator
}

fn run_child(arm: Arm, len: usize, repeats: usize) {
    let elements = corpus(CORPUS_COUNT, len);
    let refs: Vec<&[u8]> = elements.iter().map(Vec::as_slice).collect();
    let no_collator = black_box(None::<&CollatorBorrowed<'static>>);
    let result = match arm {
        Arm::Orig => run_loop(&refs, repeats, |left, right| {
            orig_sort_alpha_compare(no_collator, left, right)
        }),
        Arm::Candidate => run_loop(&refs, repeats, |left, right| {
            candidate_sort_alpha_compare(no_collator, left, right)
        }),
    };
    black_box(result);
}

fn child_args() -> Result<Option<(Arm, usize, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--child") {
        return Ok(None);
    }
    let arm = Arm::parse(args.get(2).ok_or("missing child arm")?)?;
    let len = args
        .get(3)
        .ok_or("missing child element length")?
        .parse()
        .map_err(|error| format!("invalid child element length: {error}"))?;
    let repeats = args
        .get(4)
        .ok_or("missing child repeat count")?
        .parse()
        .map_err(|error| format!("invalid child repeat count: {error}"))?;
    Ok(Some((arm, len, repeats)))
}

fn child_command(executable: &Path, arm: Arm, len: usize, repeats: usize) -> Command {
    let mut command = Command::new(executable);
    command.args([
        "--child",
        arm.name(),
        &len.to_string(),
        &repeats.to_string(),
    ]);
    command
}

fn run_warmup(executable: &Path) -> Result<(), String> {
    for arm in [Arm::Orig, Arm::Candidate, Arm::Candidate, Arm::Orig] {
        let status = child_command(executable, arm, STAT_LEN, 1_000)
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !status.success() {
            return Err(format!("{} warm-up failed with {status}", arm.name()));
        }
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args([
            "--child",
            arm.name(),
            &STAT_LEN.to_string(),
            &STAT_REPEATS.to_string(),
        ])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat for {} failed: {stderr}", arm.name()));
    }
    for line in stderr.lines() {
        let columns: Vec<_> = line.split(',').collect();
        if columns
            .iter()
            .any(|field| field.trim().contains("instructions"))
        {
            let raw = columns[0].trim();
            if raw.starts_with('<') {
                return Err(format!("perf counter unavailable: {line}"));
            }
            return raw
                .parse()
                .map_err(|error| format!("invalid perf count {raw:?}: {error}"));
        }
    }
    Err(format!("instructions:u missing from perf output: {stderr}"))
}

fn profile_orig(executable: &Path) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!("fr_sort_alpha_orig_{}_{stamp}.data", process::id()));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let output = Command::new("perf")
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
            Arm::Orig.name(),
            &STAT_LEN.to_string(),
            &PROFILE_REPEATS.to_string(),
        ])
        .output()
        .map_err(|error| format!("could not launch perf record: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&output.stderr)
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
    println!("PROFILE_TABLE_BEGIN\n{stdout}\nPROFILE_TABLE_END");
    let line = stdout
        .lines()
        .find(|line| line.contains("core::str::converts::from_utf8"))
        .ok_or("ORIG profile has no from_utf8 frame; benchmark is dead-code INVALID")?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing from_utf8 self percentage")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid from_utf8 self percentage: {error}"))?;
    if self_pct <= 0.0 {
        return Err("ORIG from_utf8 self-time is zero; benchmark is INVALID".into());
    }
    Ok(self_pct)
}

fn mean_cv(samples: &[f64]) -> Result<(f64, f64), String> {
    if samples.len() < 2 {
        return Err("need at least two samples".into());
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    if mean <= 0.0 {
        return Err("sample mean must be positive".into());
    }
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / (samples.len() - 1) as f64;
    Ok((mean, variance.sqrt() / mean * 100.0))
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    let middle = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2.0
    } else {
        samples[middle]
    }
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

fn bootstrap_median_ci(samples: &[f64]) -> (f64, f64) {
    let mut state = 0x5eed_f00d_cafe_babe_u64;
    for sample in samples {
        state ^= sample.to_bits().wrapping_mul(0x9e37_79b9_7f4a_7c15);
        state = state.rotate_left(17);
    }
    let mut scratch = vec![0.0; samples.len()];
    let mut medians = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        for value in &mut scratch {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let draw = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            *value = samples[(draw as usize) % samples.len()];
        }
        medians.push(median(&mut scratch));
    }
    medians.sort_by(f64::total_cmp);
    (percentile(&medians, 0.025), percentile(&medians, 0.975))
}

fn executable_sha256(executable: &Path) -> Result<String, String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = fs::read(executable)
        .map_err(|error| format!("could not read {}: {error}", executable.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut digest = String::with_capacity(64);
    for byte in hasher.finalize() {
        digest.push(char::from(HEX[usize::from(byte >> 4)]));
        digest.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(digest)
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    println!(
        "A_B_HOST={} executable={}",
        env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
        executable.display()
    );
    let from_utf8_self_pct = profile_orig(executable)?;
    println!("ORIG_REACHABILITY from_utf8_self_pct={from_utf8_self_pct:.4}");
    run_warmup(executable)?;

    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    let mut orig = Vec::with_capacity(STAT_ROUNDS);
    let mut candidate = Vec::with_capacity(STAT_ROUNDS);
    for round in 0..STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 { Arm::Candidate } else { Arm::Orig };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null = counts[0] as f64 / counts[1] as f64;
        let effect = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} orig_a={} orig_b={} candidate={} \
null_ratio={null:.9} candidate_over_orig={effect:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        nulls.push(null);
        effects.push(effect);
        orig.push(counts[0] as f64);
        candidate.push(counts[2] as f64);
    }

    let (orig_mean, orig_cv_pct) = mean_cv(&orig)?;
    let (candidate_mean, candidate_cv_pct) = mean_cv(&candidate)?;
    let (_, null_cv_pct) = mean_cv(&nulls)?;
    let (_, effect_cv_pct) = mean_cv(&effects)?;
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let (null_ci95_low, null_ci95_high) = bootstrap_median_ci(&nulls);
    let (effect_ci95_low, effect_ci95_high) = bootstrap_median_ci(&effects);
    let null_radius = (1.0 - null_ci95_low)
        .abs()
        .max((null_ci95_high - 1.0).abs());
    let gate_low = 1.0 - 2.0 * null_radius;
    println!(
        "INSTRUCTIONS_SUMMARY orig_mean={orig_mean:.3} orig_cv_pct={orig_cv_pct:.6} \
candidate_mean={candidate_mean:.3} candidate_cv_pct={candidate_cv_pct:.6} \
null_median={null_median:.9} null_ci95_low={null_ci95_low:.9} \
null_ci95_high={null_ci95_high:.9} null_cv_pct={null_cv_pct:.6} \
candidate_over_orig_median={effect_median:.9} effect_ci95_low={effect_ci95_low:.9} \
effect_ci95_high={effect_ci95_high:.9} effect_cv_pct={effect_cv_pct:.6} \
margin2x_low={gate_low:.9} bootstrap_resamples={BOOTSTRAP_RESAMPLES} \
cv_used_as_provenance_only=true"
    );
    if !(null_ci95_low <= 1.0 && 1.0 <= null_ci95_high) {
        return Err(format!(
            "A/A bootstrap median CI does not bracket 1.0: \
[{null_ci95_low:.9}, {null_ci95_high:.9}]"
        ));
    }
    if effect_ci95_high >= gate_low || effect_median >= KEEP_GATE_RATIO {
        return Err(format!(
            "median-CI keep gate failed: candidate/orig median={effect_median:.9}, \
CI high={effect_ci95_high:.9}, 2x null floor={gate_low:.9}"
        ));
    }
    println!(
        "DECISION verdict=KEEP gate=bootstrap_median_ci95 two_x_margin=true \
cv_used_as_provenance_only=true"
    );
    Ok(())
}

fn run_criterion(c: &mut Criterion) {
    let mut group = c.benchmark_group("sort_alpha_compare_abba");
    for &len in &[8_usize, 32, 128] {
        let elements = corpus(512, len);
        let refs: Vec<&[u8]> = elements.iter().map(Vec::as_slice).collect();
        let no_collator = black_box(None::<&CollatorBorrowed<'static>>);
        for round in 0..2 {
            let order = if round == 0 {
                [Arm::Orig, Arm::Candidate]
            } else {
                [Arm::Candidate, Arm::Orig]
            };
            for arm in order {
                group.bench_with_input(
                    BenchmarkId::new(format!("round_{round}_{}", arm.name()), len),
                    &refs,
                    |b, refs| {
                        b.iter(|| match arm {
                            Arm::Orig => run_loop(refs, 1, |left, right| {
                                orig_sort_alpha_compare(no_collator, left, right)
                            }),
                            Arm::Candidate => run_loop(refs, 1, |left, right| {
                                candidate_sort_alpha_compare(no_collator, left, right)
                            }),
                        })
                    },
                );
            }
        }
    }
    group.finish();
}

fn main() {
    match child_args() {
        Ok(Some((arm, len, repeats))) => {
            run_child(arm, len, repeats);
            return;
        }
        Ok(None) => {}
        Err(error) => panic!("invalid child arguments: {error}"),
    }

    let executable: PathBuf = env::current_exe().expect("current bench executable path");
    let sha256 =
        executable_sha256(&executable).unwrap_or_else(|error| panic!("ELF hash failed: {error}"));
    println!(
        "BENCH_ELF_SHA256 arms=orig_a,orig_b,candidate sha256={sha256} bytes={}",
        fs::metadata(&executable)
            .expect("bench executable metadata")
            .len()
    );
    run_instruction_ab(&executable).unwrap_or_else(|error| panic!("A/B INVALID: {error}"));
    let mut criterion = Criterion::default().configure_from_args();
    run_criterion(&mut criterion);
    criterion.final_summary();
}
