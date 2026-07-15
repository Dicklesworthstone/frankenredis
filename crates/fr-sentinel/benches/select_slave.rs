//! Same-binary proof for Sentinel replica selection.
//!
//! The frozen reference owns every eligible score and stably sorts all of them. The candidate
//! selects the same first minimum while borrowing scores and owns only the winning key.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_sentinel::{
    InstanceFlags, SentinelAddr, SentinelRedisInstance,
    failover::{bench_select_slave_sort_all_reference, select_slave_at},
};

const NOW_MS: u64 = 100_000;
const PING_PERIOD_MS: u64 = 1_000;
const INFO_PERIOD_MS: u64 = 10_000;
const REPLICAS: usize = 128;
const PROFILE_REFERENCE_REPEATS: usize = 20_000;
const PROFILE_CANDIDATE_REPEATS: usize = 80_000;
const STAT_REPEATS: usize = 5_000;
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

    const fn profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "fr_sentinel::failover::select_slave_at",
            Self::Reference => "bench_select_slave_sort_all_reference",
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "bench_select_slave_sort_all_reference",
            Self::Reference => "fr_sentinel::failover::select_slave_at",
        }
    }

    const fn profile_repeats(self) -> usize {
        match self {
            Self::Candidate => PROFILE_CANDIDATE_REPEATS,
            Self::Reference => PROFILE_REFERENCE_REPEATS,
        }
    }
}

fn master_with_replicas(count: usize) -> SentinelRedisInstance {
    let mut master =
        SentinelRedisInstance::new_master("mymaster", SentinelAddr::new("10.0.0.1", 6_379), 2);
    for index in 0..count {
        let key = format!("10.0.1.{index}:6379");
        let mut replica = SentinelRedisInstance::new_master(
            key.clone(),
            SentinelAddr::new(format!("10.0.1.{index}"), 6_379),
            0,
        );
        replica.flags = InstanceFlags::SLAVE;
        replica.slave_priority = 100;
        replica.slave_repl_offset = (index % 8) as u64 * 1_000;
        replica.runid = Some(format!("replica-runid-{index:04}"));
        replica.link.last_avail_time = NOW_MS;
        replica.info_refresh = NOW_MS;
        master.slaves.insert(key, replica);
    }
    master
}

fn select(master: &SentinelRedisInstance, arm: Arm) -> Option<String> {
    match arm {
        Arm::Candidate => select_slave_at(
            black_box(master),
            black_box(NOW_MS),
            black_box(PING_PERIOD_MS),
            black_box(INFO_PERIOD_MS),
        ),
        Arm::Reference => bench_select_slave_sort_all_reference(
            black_box(master),
            black_box(NOW_MS),
            black_box(PING_PERIOD_MS),
            black_box(INFO_PERIOD_MS),
        ),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let master = master_with_replicas(REPLICAS);
    let mut checksum = 0_usize;
    for _ in 0..repeats {
        let selected = select(black_box(&master), arm);
        checksum = checksum.wrapping_add(black_box(&selected).as_ref().map_or(0, String::len));
        black_box(selected);
    }
    black_box(checksum);
}

fn assert_same(master: &SentinelRedisInstance, label: &str) {
    assert_eq!(
        select(master, Arm::Candidate),
        select(master, Arm::Reference),
        "selection differs for {label}"
    );
}

fn correctness_gate() {
    for count in [0, 1, 2, 8, REPLICAS] {
        assert_same(&master_with_replicas(count), &format!("count={count}"));
    }

    let mut filtered = master_with_replicas(8);
    let keys: Vec<_> = filtered.slaves.keys().cloned().collect();
    filtered.slaves.get_mut(&keys[0]).unwrap().link.disconnected = true;
    filtered.slaves.get_mut(&keys[1]).unwrap().slave_priority = 0;
    filtered
        .slaves
        .get_mut(&keys[2])
        .unwrap()
        .flags
        .insert(InstanceFlags::S_DOWN);
    filtered.slaves.get_mut(&keys[3]).unwrap().info_refresh = 0;
    assert_same(&filtered, "eligibility filters");

    let mut exact_ties = master_with_replicas(8);
    for replica in exact_ties.slaves.values_mut() {
        replica.slave_repl_offset = 42;
        replica.runid = Some("same-runid".to_owned());
    }
    assert_same(&exact_ties, "stable exact-score tie");

    println!(
        "CORRECTNESS_GATE selection=identical counts=0,1,2,8,128 eligibility_filters=covered stable_exact_tie=preserved"
    );
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
        "fr_sentinel_select_slave_{}_{}_{}.data",
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
        .args(["--child", arm.name(), &arm.profile_repeats().to_string()])
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
    if stdout
        .lines()
        .any(|line| line.contains(arm.wrong_profile_symbol()))
    {
        return Err(format!(
            "{} profile executed wrong helper {}",
            arm.name(),
            arm.wrong_profile_symbol()
        ));
    }
    let line = stdout
        .lines()
        .find(|line| line.contains(arm.profile_symbol()) && !line.contains("closure#"))
        .ok_or_else(|| {
            format!(
                "profile has no exact {} helper frame; workload INVALID",
                arm.name()
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
        return Err(format!("{} helper has zero self-time", arm.name()));
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path, arms: &[Arm]) -> Result<(), String> {
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!("TRIGGER eligible_replicas={REPLICAS} priority=100 offset_buckets=8 runid=present");
    for &arm in arms {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "100"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
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

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = child_args()? {
        run_loop(arm, repeats);
        return Ok(());
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    correctness_gate();
    let reference_profile_only = env::args().any(|arg| arg == "--profile-reference-only");
    if reference_profile_only {
        return run_profile(&executable, &[Arm::Reference])
            .map_err(|error| format!("PROFILE INVALID: {error}"));
    }
    run_profile(&executable, &[Arm::Candidate, Arm::Reference])
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
