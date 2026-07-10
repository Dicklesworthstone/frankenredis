//! Same-binary A/B for the `SORT ... ALPHA` comparator.
//!
//! One parent invocation profiles the bench-only ORIG arm, alternates eight ORIG/CAND
//! `perf stat instructions:u` pairs, checks CV and the keep gate, then runs AB/BA Criterion
//! blocks in one group. This is deliberately a custom `harness = false` main: separate Cargo
//! invocations would let RCH choose different workers and invalidate the ratio.

use std::{
    cmp::Ordering,
    env,
    hint::black_box,
    path::{Path, PathBuf},
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use criterion::{BenchmarkId, Criterion};
use icu_collator::CollatorBorrowed;

const PROFILE_REPEATS: usize = 50;
const STAT_REPEATS: usize = 100;
const STAT_PAIRS: usize = 8;
const STAT_LEN: usize = 32;
const CORPUS_COUNT: usize = 1_000;
const KEEP_GATE_RATIO: f64 = 0.99;
const MAX_CV_PCT: f64 = 5.0;

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

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    println!(
        "A_B_HOST={} executable={}",
        env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
        executable.display()
    );
    let from_utf8_self_pct = profile_orig(executable)?;
    println!("ORIG_REACHABILITY from_utf8_self_pct={from_utf8_self_pct:.4}");
    run_warmup(executable)?;

    let mut orig = Vec::with_capacity(STAT_PAIRS);
    let mut candidate = Vec::with_capacity(STAT_PAIRS);
    for pair in 0..STAT_PAIRS {
        let order = if pair % 2 == 0 {
            [Arm::Orig, Arm::Candidate]
        } else {
            [Arm::Candidate, Arm::Orig]
        };
        let mut pair_orig = None;
        let mut pair_candidate = None;
        for arm in order {
            let count = perf_instructions(executable, arm)?;
            match arm {
                Arm::Orig => pair_orig = Some(count),
                Arm::Candidate => pair_candidate = Some(count),
            }
        }
        let pair_orig = pair_orig.ok_or("missing ORIG count")?;
        let pair_candidate = pair_candidate.ok_or("missing candidate count")?;
        println!(
            "INSTRUCTIONS pair={} order={}/{} orig={} candidate={} candidate_over_orig={:.9}",
            pair + 1,
            order[0].name(),
            order[1].name(),
            pair_orig,
            pair_candidate,
            pair_candidate as f64 / pair_orig as f64
        );
        orig.push(pair_orig as f64);
        candidate.push(pair_candidate as f64);
    }

    let ratios: Vec<f64> = candidate
        .iter()
        .zip(&orig)
        .map(|(candidate, orig)| candidate / orig)
        .collect();
    let (orig_mean, orig_cv_pct) = mean_cv(&orig)?;
    let (candidate_mean, candidate_cv_pct) = mean_cv(&candidate)?;
    let (ratio_mean, ratio_cv_pct) = mean_cv(&ratios)?;
    println!(
        "INSTRUCTIONS_SUMMARY orig_mean={orig_mean:.3} orig_cv_pct={orig_cv_pct:.6} \
candidate_mean={candidate_mean:.3} candidate_cv_pct={candidate_cv_pct:.6} \
candidate_over_orig={ratio_mean:.9} ratio_cv_pct={ratio_cv_pct:.6}"
    );
    if orig_cv_pct >= MAX_CV_PCT || candidate_cv_pct >= MAX_CV_PCT || ratio_cv_pct >= MAX_CV_PCT {
        return Err(format!(
            "CV gate failed: orig={orig_cv_pct:.3}% candidate={candidate_cv_pct:.3}% ratio={ratio_cv_pct:.3}%"
        ));
    }
    if ratio_mean >= KEEP_GATE_RATIO {
        return Err(format!(
            "1% instruction keep gate failed: candidate/orig={ratio_mean:.9}"
        ));
    }
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
    run_instruction_ab(&executable).unwrap_or_else(|error| panic!("A/B INVALID: {error}"));
    let mut criterion = Criterion::default().configure_from_args();
    run_criterion(&mut criterion);
    criterion.final_summary();
}
