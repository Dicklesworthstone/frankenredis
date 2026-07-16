//! Same-binary proof for stale Sentinel peer pruning.
//!
//! The candidate retains live entries in one table pass. The frozen reference collects owned
//! stale keys and hashes each of them again for removal.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fr_sentinel::{
    InstanceFlags, SentinelAddr, SentinelRedisInstance,
    discovery::{bench_prune_stale_sentinels_collect_reference, prune_stale_sentinels},
};

const NOW_MS: u64 = 1_000_000;
const MAX_AGE_MS: u64 = 60_000;
const SENTINELS_PER_MASTER: usize = 512;
const PROFILE_MASTERS: usize = 256;
const STAT_MASTERS: usize = 64;
const STAT_ROUNDS: usize = 9;
const PERF_DELAY_MS: u64 = 1_000;
const CHILD_WAIT_MS: u64 = 1_100;

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
            Self::Candidate => "fr_sentinel::discovery::prune_stale_sentinels",
            Self::Reference => {
                "fr_sentinel::discovery::bench_prune_stale_sentinels_collect_reference"
            }
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => {
                "fr_sentinel::discovery::bench_prune_stale_sentinels_collect_reference"
            }
            Self::Reference => "fr_sentinel::discovery::prune_stale_sentinels",
        }
    }
}

fn master_with_sentinels(batch: usize) -> SentinelRedisInstance {
    let mut master = SentinelRedisInstance::new_master(
        format!("master-{batch:04}"),
        SentinelAddr::new(format!("10.0.{}.1", batch % 255), 6_379),
        2,
    );
    master.sentinels.reserve(SENTINELS_PER_MASTER);
    for index in 0..SENTINELS_PER_MASTER {
        let key = format!("sentinel-{batch:04}-{index:04}.cluster.example.invalid:26379");
        let host = format!("sentinel-{batch:04}-{index:04}.cluster.example.invalid");
        let mut sentinel =
            SentinelRedisInstance::new_master(key.clone(), SentinelAddr::new(host, 26_379), 0);
        sentinel.flags = InstanceFlags::SENTINEL;
        sentinel.last_hello_time = if index % 2 == 0 {
            NOW_MS - MAX_AGE_MS - 1 - (index as u64 % 1_000)
        } else {
            NOW_MS - (index as u64 % MAX_AGE_MS)
        };
        master.sentinels.insert(key, sentinel);
    }
    master
}

fn prune(master: &mut SentinelRedisInstance, arm: Arm, now: u64, max_age_ms: u64) {
    match arm {
        Arm::Candidate => {
            prune_stale_sentinels(black_box(master), black_box(now), black_box(max_age_ms))
        }
        Arm::Reference => bench_prune_stale_sentinels_collect_reference(
            black_box(master),
            black_box(now),
            black_box(max_age_ms),
        ),
    }
}

fn run_child(arm: Arm, master_count: usize) -> ! {
    let mut masters: Vec<_> = (0..master_count).map(master_with_sentinels).collect();
    // `perf --delay` excludes fixture construction. This wait keeps the process alive until the
    // PMU is enabled and therefore measures only the destructive prune plus child exit.
    thread::sleep(Duration::from_millis(CHILD_WAIT_MS));
    let mut checksum = 0_usize;
    for master in &mut masters {
        prune(master, arm, NOW_MS, MAX_AGE_MS);
        checksum = checksum.wrapping_add(black_box(master.sentinels.len()));
    }
    black_box(checksum);
    process::exit(0);
}

fn child_args() -> Result<Option<(Arm, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--child") {
        return Ok(None);
    }
    let arm = Arm::parse(args.get(2).ok_or("missing child arm")?)?;
    let master_count = args
        .get(3)
        .ok_or("missing child master count")?
        .parse()
        .map_err(|error| format!("invalid child master count: {error}"))?;
    Ok(Some((arm, master_count)))
}

fn master_with_times(times: &[u64]) -> SentinelRedisInstance {
    let mut master =
        SentinelRedisInstance::new_master("parity-master", SentinelAddr::new("10.0.0.1", 6_379), 2);
    for (index, &last_hello_time) in times.iter().enumerate() {
        let key = format!("sentinel-{index:03}:26379");
        let mut sentinel = SentinelRedisInstance::new_master(
            key.clone(),
            SentinelAddr::new(format!("sentinel-{index:03}"), 26_379),
            0,
        );
        sentinel.flags = InstanceFlags::SENTINEL;
        sentinel.last_hello_time = last_hello_time;
        sentinel.runid = Some(format!("runid-{index:03}"));
        master.sentinels.insert(key, sentinel);
    }
    master
}

fn survivor_snapshot(master: &SentinelRedisInstance) -> Vec<(String, String, u64, String, u16)> {
    master
        .sentinels
        .iter()
        .map(|(key, sentinel)| {
            (
                key.clone(),
                sentinel.name.clone(),
                sentinel.last_hello_time,
                sentinel.addr.hostname.clone(),
                sentinel.addr.port,
            )
        })
        .collect()
}

fn assert_same(times: &[u64], now: u64, max_age_ms: u64, label: &str) {
    let original = master_with_times(times);
    let mut candidate = original.clone();
    let mut reference = original;
    prune(&mut candidate, Arm::Candidate, now, max_age_ms);
    prune(&mut reference, Arm::Reference, now, max_age_ms);
    assert_eq!(
        survivor_snapshot(&candidate),
        survivor_snapshot(&reference),
        "survivor values or iteration order differ for {label}"
    );
}

fn correctness_gate() {
    assert_same(&[], NOW_MS, MAX_AGE_MS, "empty");
    assert_same(
        &[NOW_MS, NOW_MS - MAX_AGE_MS, NOW_MS + 1],
        NOW_MS,
        MAX_AGE_MS,
        "all live including boundary and future timestamp",
    );
    assert_same(
        &[0, NOW_MS - MAX_AGE_MS - 1, 1],
        NOW_MS,
        MAX_AGE_MS,
        "all stale",
    );
    assert_same(
        &[
            0,
            NOW_MS,
            NOW_MS - MAX_AGE_MS,
            NOW_MS - MAX_AGE_MS - 1,
            NOW_MS + 1,
        ],
        NOW_MS,
        MAX_AGE_MS,
        "mixed boundaries",
    );
    assert_same(&[NOW_MS - 1, NOW_MS, NOW_MS + 1], NOW_MS, 0, "zero max age");
    assert_same(&[u64::MAX], 0, 0, "saturating future timestamp");
    println!(
        "CORRECTNESS_GATE result=identical cases=6 empty_live_stale_mixed_boundary_future_and_zero_age=covered iteration_order=preserved"
    );
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
        "fr_sentinel_stale_prune_{}_{}_{stamp}.data",
        process::id(),
        arm.name()
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let recorded = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "record",
            "-q",
            "--delay",
            &PERF_DELAY_MS.to_string(),
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
        .args(["--child", arm.name(), &PROFILE_MASTERS.to_string()])
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
        // LLVM can fold the outer helper into a sampled closure (for example, the candidate's
        // `retain` predicate). Such a child frame is still unambiguous proof that this arm ran.
        .find(|line| line.contains(arm.profile_symbol()))
        .ok_or_else(|| {
            format!(
                "profile has no {} frame family; workload INVALID",
                arm.profile_symbol()
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
        return Err(format!("{} has zero self-time", arm.profile_symbol()));
    }
    Ok(self_pct)
}

fn run_profiles(executable: &Path) -> Result<(), String> {
    for arm in [Arm::Candidate, Arm::Reference] {
        let self_pct = profile_trial(executable, arm)?;
        println!(
            "PROFILE_SELF arm={} symbol={} self_pct={self_pct:.4}",
            arm.name(),
            arm.profile_symbol()
        );
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "stat",
            "--delay",
            &PERF_DELAY_MS.to_string(),
            "--no-big-num",
            "-x,",
            "-e",
            "instructions:u",
            "--",
        ])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_MASTERS.to_string()])
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
    let fewer_instructions_pct = 100.0 * (1.0 - candidate_median / reference_median);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} fewer_instructions_pct={fewer_instructions_pct:.6} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv_pct:.6}"
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
    println!("DECISION keep=true effect={effect_median:.9} null_p95={null_p95:.9}");
    Ok(())
}

fn main() -> Result<(), String> {
    if let Some((arm, master_count)) = child_args()? {
        run_child(arm, master_count);
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    correctness_gate();
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(&executable)?);
    println!(
        "TRIGGER sentinels_per_master={SENTINELS_PER_MASTER} profile_masters={PROFILE_MASTERS} stat_masters={STAT_MASTERS} stale_fraction=0.5 max_age_ms={MAX_AGE_MS} setup_excluded_by_perf_delay=true"
    );
    run_profiles(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
