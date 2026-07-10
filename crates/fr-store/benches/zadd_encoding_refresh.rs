//! Profile-first harness for the packed-ZSET encoding refresh after an existing-member ZADD.
//!
//! The workload keeps 96 short members in the listpack-compatible representation and alternates
//! one member's score, so every operation is a real mutation and reaches the sticky encoding
//! refresh. The parent runs several `perf record` trials inside this one remote bench invocation
//! and gates further work on the median self-time of `Store::refresh_zset_encoding_flag`. The
//! fallback arm selects the exact pre-change refresh inside the already-borrowed ZADD entry.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_store::Store;

const MEMBER_COUNT: usize = 96;
const PROFILE_REPEATS: usize = 250_000;
const PROFILE_TRIALS: usize = 3;
const STAT_REPEATS: usize = 50_000;
const STAT_ROUNDS: usize = 24;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

#[derive(Clone, Copy, Debug)]
enum Arm {
    Candidate,
    Fallback,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Fallback => "fallback",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "fallback" => Ok(Self::Fallback),
            _ => Err(format!("unknown child arm {value:?}")),
        }
    }
}

fn seed_store() -> Store {
    let mut store = Store::new();
    let members: Vec<(f64, Vec<u8>)> = (0..MEMBER_COUNT)
        .map(|i| (i as f64, format!("member:{i:03}").into_bytes()))
        .collect();
    assert_eq!(
        store.zadd_plain_owned(b"z", members, 1).expect("seed ZADD"),
        MEMBER_COUNT
    );
    store
}

fn apply_update(store: &mut Store, arm: Arm, i: usize) -> usize {
    let score = if i & 1 == 0 { 48.25 } else { 48.75 };
    let members = vec![(black_box(score), black_box(b"member:048".to_vec()))];
    let added = match arm {
        Arm::Candidate => store.zadd_plain_owned(black_box(b"z"), members, 2),
        Arm::Fallback => store.bench_zadd_plain_owned_fallback(black_box(b"z"), members, 2),
    };
    added.expect("profile ZADD")
}

fn run_mutating_loop(arm: Arm, repeats: usize) {
    let mut store = seed_store();
    let mut checksum = 0usize;
    for i in 0..repeats {
        checksum = checksum.wrapping_add(apply_update(&mut store, arm, i));
    }
    black_box(checksum);
    black_box(store.zscore(b"z", b"member:048", 2).expect("final ZSCORE"));
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
        .map_err(|error| format!("invalid child repeat count: {error}"))?;
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

fn profile_trial(executable: &Path, arm: Arm, trial: usize) -> Result<(f64, f64), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_zadd_encoding_refresh_{}_{}_{}_{}.data",
        process::id(),
        arm.name(),
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
    println!(
        "PROFILE_TABLE_BEGIN arm={} trial={trial}\n{stdout}\nPROFILE_TABLE_END arm={} trial={trial}",
        arm.name(),
        arm.name()
    );
    let zadd_core_line = stdout
        .lines()
        .find(|line| line.contains("<fr_store::SortedSet>::insert_with_limits_result"))
        .ok_or("profile has no insert_with_limits_result frame; workload is INVALID")?;
    let zadd_core_self_pct = zadd_core_line
        .split_whitespace()
        .next()
        .ok_or("missing ZADD-core self percentage")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid ZADD-core self percentage: {error}"))?;
    let fallback_self_pct = stdout
        .lines()
        .find(|line| {
            line.contains("<fr_store::Store>::refresh_zset_encoding_flag")
                && !line.contains("after_insert")
        })
        .map(|line| {
            line.split_whitespace()
                .next()
                .ok_or("missing fallback self percentage")?
                .trim_end_matches('%')
                .parse::<f64>()
                .map_err(|error| format!("invalid fallback self percentage: {error}"))
        })
        .transpose()?
        .unwrap_or(0.0);
    if zadd_core_self_pct <= 0.0 {
        return Err("insert_with_limits_result has zero self-time; workload is INVALID".into());
    }
    match arm {
        Arm::Fallback if fallback_self_pct <= 0.0 => {
            Err("fallback profile has no full refresh frame; reference is INVALID".into())
        }
        Arm::Candidate if fallback_self_pct > 0.0 => Err(format!(
            "candidate still reaches the full refresh scan ({fallback_self_pct:.4}% self)"
        )),
        _ => Ok((zadd_core_self_pct, fallback_self_pct)),
    }
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| {
        left.partial_cmp(right)
            .expect("profile self-time is not NaN")
    });
    samples[samples.len() / 2]
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
    println!("BINARY_SHA256 control={}", binary_sha256(executable)?);
    for arm in [Arm::Fallback, Arm::Candidate] {
        let warm = Command::new(executable)
            .args(["--child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed with {warm}", arm.name()));
        }
    }

    let mut fallback_samples = Vec::with_capacity(PROFILE_TRIALS);
    for arm in [Arm::Fallback, Arm::Candidate] {
        for trial in 1..=PROFILE_TRIALS {
            let (zadd_core_self_pct, fallback_self_pct) = profile_trial(executable, arm, trial)?;
            println!(
                "PROFILE_SELF arm={} trial={trial} zadd_core_self_pct={zadd_core_self_pct:.4} \
fallback_refresh_self_pct={fallback_self_pct:.4}",
                arm.name()
            );
            if matches!(arm, Arm::Fallback) {
                fallback_samples.push(fallback_self_pct);
            }
        }
    }
    let median_self_pct = median(&mut fallback_samples);
    println!(
        "PROFILE_SELF_SUMMARY arm=fallback trials={PROFILE_TRIALS} median_self_pct={median_self_pct:.4} \
samples={fallback_samples:?} candidate_full_scan_reported_self_pct=0.0000 report_floor_pct=0.1000"
    );
    if median_self_pct <= 0.1 {
        return Err(format!(
            "median refresh self-time {median_self_pct:.4}% does not clear the 0.1% attribution floor"
        ));
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
        .map_err(|error| format!("could not launch perf stat for {}: {error}", arm.name()))?;
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

fn cv(samples: &[f64]) -> f64 {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    100.0 * variance.sqrt() / mean
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn correctness_gate() {
    let mut candidate = seed_store();
    let mut fallback = seed_store();
    for i in 0..101 {
        assert_eq!(
            apply_update(&mut candidate, Arm::Candidate, i),
            apply_update(&mut fallback, Arm::Fallback, i),
            "ZADD reply differs at update {i}"
        );
    }
    assert_eq!(
        candidate.zrange_withscores(b"z", 0, -1, 3),
        fallback.zrange_withscores(b"z", 0, -1, 3),
        "sorted-set contents/order differ"
    );
    assert_eq!(
        candidate.object_encoding(b"z", 3),
        fallback.object_encoding(b"z", 3),
        "OBJECT ENCODING differs"
    );
    assert_eq!(
        candidate.dump_key(b"z", 3),
        fallback.dump_key(b"z", 3),
        "DUMP bytes differ"
    );
    println!("CORRECTNESS_GATE zadd_reply_zrange_scores_encoding_dump=identical");
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut null_ratios = Vec::with_capacity(STAT_ROUNDS);
    let mut speedups = Vec::with_capacity(STAT_ROUNDS);
    for round in 0..STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                Arm::Fallback
            } else {
                Arm::Candidate
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null_ratio = counts[0] as f64 / counts[1] as f64;
        let speedup = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} fallback={} \
null_ratio={null_ratio:.9} fallback_over_candidate={speedup:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        null_ratios.push(null_ratio);
        speedups.push(speedup);
    }

    let null_cv_pct = cv(&null_ratios);
    let speedup_cv_pct = cv(&speedups);
    let null_median = median(&mut null_ratios);
    let speedup_median = median(&mut speedups);
    let null_p05 = percentile(&null_ratios, NULL_LO);
    let null_p95 = percentile(&null_ratios, NULL_HI);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} \
null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} \
fallback_over_candidate_median={speedup_median:.9} speedup_cv_pct={speedup_cv_pct:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!(
            "null median exposes harness bias: {null_median:.9}"
        ));
    }
    if speedup_median <= null_p95 {
        return Err(format!(
            "candidate median does not clear null spread: speedup={speedup_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    if speedup_median <= 1.01 {
        return Err(format!(
            "1% instruction keep gate failed: {speedup_median:.9}x"
        ));
    }
    Ok(())
}

fn main() {
    match child_args() {
        Ok(Some((arm, repeats))) => {
            run_mutating_loop(arm, repeats);
            return;
        }
        Ok(None) => {}
        Err(error) => panic!("invalid child arguments: {error}"),
    }
    let executable = env::current_exe().expect("current bench executable path");
    correctness_gate();
    run_profile(&executable).unwrap_or_else(|error| panic!("PROFILE INVALID: {error}"));
    run_instruction_ab(&executable).unwrap_or_else(|error| panic!("A/B INVALID: {error}"));
}
