//! Profile-first, same-binary benchmark for plain packed-ZSET rank scans. Candidate and exact
//! pre-change reference are const-generic arms in this executable; a missing member forces all 120
//! listpack-compatible records to be scanned. The parent profiles both arms, then runs interleaved
//! A/A/reference instruction counts so worker drift and harness bias are explicit.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_store::Store;

const MEMBER_COUNT: usize = 120;
const PROFILE_REPEATS: usize = 250_000;
const PROFILE_TRIALS: usize = 3;
const STAT_REPEATS: usize = 100_000;
const STAT_ROUNDS: usize = 24;

#[derive(Clone, Copy, Debug)]
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
            _ => Err(format!("unknown child arm {value:?}")),
        }
    }
}

fn seed_store() -> Store {
    let mut store = Store::new();
    let members: Vec<(f64, Vec<u8>)> = (0..120u32)
        .map(|i| (f64::from(i), format!("member:{i:04}").into_bytes()))
        .collect();
    assert_eq!(
        store.zadd(b"z", &members, 1).expect("seed ZADD"),
        MEMBER_COUNT
    );
    assert_eq!(store.object_encoding(b"z", 1), Some("listpack"));
    store
}

fn run_rank(store: &mut Store, arm: Arm, member: &[u8]) -> Option<usize> {
    match arm {
        Arm::Candidate => store.zrank(black_box(b"z"), black_box(member), 1),
        Arm::Reference => {
            store.bench_zrank_decode_scores_reference(black_box(b"z"), black_box(member), 1)
        }
    }
    .expect("ZRANK")
}

fn run_rank_loop(arm: Arm, repeats: usize) {
    let mut store = seed_store();
    let mut checksum = 0usize;
    for _ in 0..repeats {
        checksum = checksum.wrapping_add(run_rank(&mut store, arm, b"member:zzzz").unwrap_or(0));
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

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| left.partial_cmp(right).expect("sample is not NaN"));
    samples[samples.len() / 2]
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

fn profile_trial(executable: &Path, arm: Arm, trial: usize) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_packed_zrank_{}_{}_{}_{}.data",
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
    let line = stdout
        .lines()
        .find(|line| line.contains("PackedZSet") && line.contains("rank_impl"))
        .ok_or("profile has no PackedZSet::rank_impl frame; workload is INVALID")?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing rank_impl self percentage")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid rank_impl self percentage: {error}"))?;
    if self_pct <= 0.0 {
        return Err("rank_impl has zero self-time; workload is INVALID".into());
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
    println!("BINARY_SHA256 control={}", binary_sha256(executable)?);

    for arm in [Arm::Reference, Arm::Candidate] {
        let warm = Command::new(executable)
            .args(["--child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed with {warm}", arm.name()));
        }
    }

    for arm in [Arm::Reference, Arm::Candidate] {
        let mut samples = Vec::with_capacity(PROFILE_TRIALS);
        for trial in 1..=PROFILE_TRIALS {
            let self_pct = profile_trial(executable, arm, trial)?;
            println!(
                "PROFILE_SELF arm={} trial={trial} rank_impl_self_pct={self_pct:.4}",
                arm.name()
            );
            samples.push(self_pct);
        }
        let self_cv_pct = cv(&samples);
        let median_self_pct = median(&mut samples);
        println!(
            "PROFILE_SELF_SUMMARY arm={} trials={PROFILE_TRIALS} median_self_pct={median_self_pct:.4} self_cv_pct={self_cv_pct:.4} samples={samples:?}",
            arm.name()
        );
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

fn correctness_gate() {
    let mut candidate = seed_store();
    let mut reference = seed_store();
    for index in 0..MEMBER_COUNT {
        let member = format!("member:{index:04}");
        assert_eq!(
            run_rank(&mut candidate, Arm::Candidate, member.as_bytes()),
            run_rank(&mut reference, Arm::Reference, member.as_bytes()),
            "rank differs for member {index}"
        );
    }
    assert_eq!(
        run_rank(&mut candidate, Arm::Candidate, b"member:zzzz"),
        run_rank(&mut reference, Arm::Reference, b"member:zzzz"),
        "missing-member rank differs"
    );
    assert_eq!(
        candidate.dump_key(b"z", 1),
        reference.dump_key(b"z", 1),
        "rank scan changed stored bytes"
    );
    println!("CORRECTNESS_GATE all_present_missing_and_dump=identical encoding=listpack");
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
                Arm::Reference
            } else {
                Arm::Candidate
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null_ratio = counts[0] as f64 / counts[1] as f64;
        let speedup = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} null_ratio={null_ratio:.9} reference_over_candidate={speedup:.9}",
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
    let null_p05 = percentile(&null_ratios, 0.05);
    let null_p95 = percentile(&null_ratios, 0.95);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={speedup_median:.9} speedup_cv_pct={speedup_cv_pct:.6}"
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

fn main() -> Result<(), String> {
    match child_args() {
        Ok(Some((arm, repeats))) => {
            run_rank_loop(arm, repeats);
            return Ok(());
        }
        Ok(None) => {}
        Err(error) => return Err(format!("invalid child arguments: {error}")),
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve current bench executable: {error}"))?;
    correctness_gate();
    run_profile(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))?;
    Ok(())
}
