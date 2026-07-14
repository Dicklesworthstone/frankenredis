//! Same-binary A/A+A/B for the packed zero-copy ascending rank slice used by
//! `ZRANGE ... WITHSCORES`. Production skips score decoding before the requested window; the
//! reference arm retains the prior `iter().skip().take()` traversal.

use std::env;
use std::hint::black_box;
use std::path::Path;
use std::process::{self, Command};
use std::time::{SystemTime, UNIX_EPOCH};

use fr_store::BenchPackedZSet;

const MEMBER_COUNT: usize = 120;
const START: usize = 112;
const COUNT: usize = 8;
const PROFILE_REPEATS: usize = 400_000;
const STAT_REPEATS: usize = 200_000;
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

fn seed_zset() -> BenchPackedZSet {
    BenchPackedZSet::from_unique_pairs(
        (0..MEMBER_COUNT)
            .map(|index| {
                (
                    format!("member:{index:04}:{}", "x".repeat(index % 5)).into_bytes(),
                    (index / 3) as f64,
                )
            })
            .collect(),
    )
}

#[inline(never)]
fn visit_slice(zset: &BenchPackedZSet, arm: Arm, start: usize, count: usize) -> u64 {
    let mut checksum = 0_u64;
    let mut visit = |member: &[u8], score: f64| {
        checksum = checksum
            .wrapping_add(black_box(member.len()) as u64)
            .wrapping_add(black_box(score).to_bits());
    };
    match arm {
        Arm::Candidate => zset.for_each_index_slice_asc(start, count, &mut visit),
        Arm::Reference => {
            zset.for_each_index_slice_asc_impl::<false>(start, count, &mut visit);
        }
    }
    black_box(checksum)
}

fn run_loop(arm: Arm, repeats: usize) {
    let zset = black_box(seed_zset());
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        checksum = checksum.wrapping_add(visit_slice(
            black_box(&zset),
            arm,
            black_box(START),
            black_box(COUNT),
        ));
    }
    black_box((zset, checksum));
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

fn profile_arm(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_packed_zset_borrow_slice_asc_{}_{}_{}.data",
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
        "PROFILE_TABLE_BEGIN arm={}\n{stdout}\nPROFILE_TABLE_END arm={}",
        arm.name(),
        arm.name()
    );
    let line = stdout
        .lines()
        .find(|line| line.contains("for_each_index_slice_asc_impl"))
        .ok_or_else(|| format!("profile has no exact {} slice frame", arm.name()))?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err(format!("{} slice has zero self-time", arm.name()));
    }
    Ok(self_pct)
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

fn collect_bits(
    zset: &BenchPackedZSet,
    arm: Arm,
    start: usize,
    count: usize,
) -> Vec<(Vec<u8>, u64)> {
    let mut out = Vec::new();
    let mut collect = |member: &[u8], score: f64| out.push((member.to_vec(), score.to_bits()));
    match arm {
        Arm::Candidate => zset.for_each_index_slice_asc(start, count, &mut collect),
        Arm::Reference => {
            zset.for_each_index_slice_asc_impl::<false>(start, count, &mut collect);
        }
    }
    out
}

fn correctness_gate() {
    let zset = seed_zset();
    for start in [0, 1, 8, 96, START, 119, 120, 121, usize::MAX] {
        for count in [0, 1, COUNT, 120, usize::MAX] {
            assert_eq!(
                collect_bits(&zset, Arm::Candidate, start, count),
                collect_bits(&zset, Arm::Reference, start, count),
                "borrowed ascending slice differs for ({start}, {count})"
            );
        }
    }
    println!("CORRECTNESS_GATE deep_boundary_empty_oversized=bit_identical encoding=packed");
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    let mut candidate_per_op = Vec::with_capacity(STAT_ROUNDS);
    let mut reference_per_op = Vec::with_capacity(STAT_ROUNDS);
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
        let null = counts[0] as f64 / counts[1].max(1) as f64;
        let candidate = (counts[0] as f64 + counts[1] as f64) / 2.0;
        let effect = counts[2] as f64 / candidate.max(1.0);
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} null_ratio={null:.9} reference_over_candidate={effect:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        nulls.push(null);
        effects.push(effect);
        candidate_per_op.push(candidate / STAT_REPEATS as f64);
        reference_per_op.push(counts[2] as f64 / STAT_REPEATS as f64);
    }
    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let candidate_median = median(&mut candidate_per_op);
    let reference_median = median(&mut reference_per_op);
    let null_p05 = percentile(&nulls, 0.05);
    let null_p95 = percentile(&nulls, 0.95);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} candidate_instr_per_op={candidate_median:.3} reference_instr_per_op={reference_median:.3} reference_over_candidate_median={effect_median:.9} speedup_cv_pct={effect_cv_pct:.6}"
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
    println!("WORKER_ID {}", hostname()?);
    println!("BINARY_SHA256 {}", binary_sha256(&executable)?);
    for arm in [Arm::Candidate, Arm::Reference] {
        let status = Command::new(&executable)
            .args(["--child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
        let self_pct = profile_arm(&executable, arm)?;
        println!("PROFILE_SELF arm={} self_pct={self_pct:.4}", arm.name());
    }
    run_instruction_ab(&executable)
}

fn hostname() -> Result<String, String> {
    let output = Command::new("hostname")
        .output()
        .map_err(|error| format!("could not launch hostname: {error}"))?;
    if !output.status.success() {
        return Err(format!("hostname failed: {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
