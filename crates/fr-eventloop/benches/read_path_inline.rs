//! Same-binary proof for cross-crate inlining of successful socket-read validation.
//!
//! Live FrankenRedis read sites pass a literal `fatal_read_error = false` after successful reads.
//! The candidate calls the production helper from a no-inline caller frame, while the frozen
//! reference retains the pre-change no-inline helper and its fourth runtime argument.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "bench-reference")]
use fr_eventloop::bench_validate_read_path_reference;
use fr_eventloop::{ReadPathError, validate_read_path};

const CURRENT_BUFFER_LEN: usize = 64 * 1024;
const NEWLY_READ_BYTES: usize = 8 * 1024;
const QUERY_BUFFER_LIMIT: usize = 1024 * 1024 * 1024;
const PROFILE_REPEATS: usize = 120_000_000;
#[cfg(feature = "bench-reference")]
const STAT_REPEATS: usize = 30_000_000;
#[cfg(feature = "bench-reference")]
const STAT_ROUNDS: usize = 9;

#[derive(Clone, Copy)]
enum Arm {
    Candidate,
    #[cfg(feature = "bench-reference")]
    Reference,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            #[cfg(feature = "bench-reference")]
            Self::Reference => "reference",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "candidate" => Ok(Self::Candidate),
            #[cfg(feature = "bench-reference")]
            "reference" => Ok(Self::Reference),
            _ => Err(format!("unknown arm {value:?}")),
        }
    }
}

#[inline(never)]
fn read_path_candidate_call(
    current_query_buffer_len: usize,
    newly_read_bytes: usize,
    query_buffer_limit: usize,
) -> Result<usize, ReadPathError> {
    validate_read_path(
        current_query_buffer_len,
        newly_read_bytes,
        query_buffer_limit,
        false,
    )
}

fn consume_result(result: Result<usize, ReadPathError>, checksum: &mut usize) {
    match black_box(result) {
        Ok(next_len) => *checksum = checksum.wrapping_add(next_len),
        Err(ReadPathError::QueryBufferLimitExceeded { observed, limit }) => {
            *checksum = checksum.wrapping_add(observed).wrapping_add(limit);
        }
        Err(ReadPathError::FatalErrorDisconnect) => {
            *checksum = checksum.wrapping_add(1);
        }
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let current = black_box(CURRENT_BUFFER_LEN);
    let newly_read = black_box(NEWLY_READ_BYTES);
    let limit = black_box(QUERY_BUFFER_LIMIT);
    let mut checksum = 0usize;
    match arm {
        Arm::Candidate => {
            for _ in 0..repeats {
                let result = read_path_candidate_call(
                    black_box(current),
                    black_box(newly_read),
                    black_box(limit),
                );
                consume_result(result, &mut checksum);
            }
        }
        #[cfg(feature = "bench-reference")]
        Arm::Reference => {
            for _ in 0..repeats {
                let result = bench_validate_read_path_reference(
                    black_box(current),
                    black_box(newly_read),
                    black_box(limit),
                    false,
                );
                consume_result(result, &mut checksum);
            }
        }
    }
    black_box(checksum);
}

#[cfg(feature = "bench-reference")]
fn correctness_gate() {
    let cases = [
        (0, 0, 0, false),
        (0, 1, QUERY_BUFFER_LIMIT, false),
        (
            CURRENT_BUFFER_LEN,
            NEWLY_READ_BYTES,
            QUERY_BUFFER_LIMIT,
            false,
        ),
        (5, 5, 10, false),
        (6, 5, 10, false),
        (11, 0, 10, false),
        (usize::MAX, 0, usize::MAX, false),
        (0, usize::MAX, usize::MAX, false),
        (usize::MAX, 1, usize::MAX, false),
        (1, usize::MAX, usize::MAX, false),
        (0, 0, usize::MAX, true),
        (usize::MAX, usize::MAX, 0, true),
    ];
    for (index, &(current, newly_read, limit, fatal)) in cases.iter().enumerate() {
        let candidate = validate_read_path(
            black_box(current),
            black_box(newly_read),
            black_box(limit),
            black_box(fatal),
        );
        let reference = bench_validate_read_path_reference(
            black_box(current),
            black_box(newly_read),
            black_box(limit),
            black_box(fatal),
        );
        assert_eq!(
            candidate, reference,
            "read-path result differs for case {index}"
        );
    }
    println!(
        "CORRECTNESS_GATE exact_results=identical cases={} fatal_precedence=covered exact_limit=covered overflow=covered usize_boundaries=covered",
        cases.len()
    );
}

#[cfg(not(feature = "bench-reference"))]
fn correctness_gate() {
    println!("CORRECTNESS_GATE deferred=profile_only_build");
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

fn measured_output(mut command: Command, label: &str) -> Result<std::process::Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("could not launch {label}: {error}"))?;
    if output.status.code() == Some(124) {
        return Err(format!("{label} exceeded its measurement cap"));
    }
    Ok(output)
}

fn profile_trial(executable: &Path, arm: Arm, live_profile: bool) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_eventloop_read_path_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }

    let mut record = Command::new("timeout");
    record
        .env("LC_ALL", "C")
        .args([
            "--foreground",
            "30s",
            "perf",
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
        .args(["--child", arm.name(), &PROFILE_REPEATS.to_string()]);
    let recorded = measured_output(record, "perf record")?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }

    let mut report = Command::new("timeout");
    report.env("LC_ALL", "C").args([
        "--foreground",
        "15s",
        "perf",
        "report",
        "-i",
        data.to_str().ok_or("non-UTF-8 perf.data path")?,
        "--stdio",
        "--no-children",
        "-g",
        "none",
        "--percent-limit",
        "0.01",
    ]);
    let report = measured_output(report, "perf report")?;
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

    let expected_symbol = if live_profile {
        "fr_eventloop::validate_read_path"
    } else {
        match arm {
            Arm::Candidate => "read_path_inline::read_path_candidate_call",
            #[cfg(feature = "bench-reference")]
            Arm::Reference => "fr_eventloop::bench_validate_read_path_reference",
        }
    };
    let line = stdout
        .lines()
        .find(|line| line.contains(expected_symbol))
        .ok_or_else(|| format!("profile has no exact {expected_symbol} frame; workload INVALID"))?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err(format!("{expected_symbol} has zero self-time"));
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path, live_profile_only: bool) -> Result<(), String> {
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!(
        "TRIGGER operation=successful_socket_read current_buffer={CURRENT_BUFFER_LEN} newly_read={NEWLY_READ_BYTES} query_buffer_limit={QUERY_BUFFER_LIMIT} fatal_read_error=false"
    );
    let candidate_status = Command::new(executable)
        .args(["--child", "candidate", "8"])
        .status()
        .map_err(|error| format!("could not launch candidate warm-up: {error}"))?;
    if !candidate_status.success() {
        return Err("candidate warm-up failed".to_owned());
    }
    let candidate_self = profile_trial(executable, Arm::Candidate, live_profile_only)?;
    println!("PROFILE_SELF arm=candidate self_pct={candidate_self:.4}");
    if live_profile_only {
        return Ok(());
    }

    #[cfg(feature = "bench-reference")]
    {
        let reference_status = Command::new(executable)
            .args(["--child", "reference", "8"])
            .status()
            .map_err(|error| format!("could not launch reference warm-up: {error}"))?;
        if !reference_status.success() {
            return Err("reference warm-up failed".to_owned());
        }
        let reference_self = profile_trial(executable, Arm::Reference, false)?;
        println!("PROFILE_SELF arm=reference self_pct={reference_self:.4}");
        Ok(())
    }
    #[cfg(not(feature = "bench-reference"))]
    Err("same-binary A/B requires bench-reference".to_owned())
}

#[cfg(feature = "bench-reference")]
fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let mut command = Command::new("timeout");
    command
        .env("LC_ALL", "C")
        .args([
            "--foreground",
            "30s",
            "perf",
            "stat",
            "--no-big-num",
            "-x,",
            "-e",
            "instructions:u",
            "--",
        ])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()]);
    let output = measured_output(command, "perf stat")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat failed: {stderr}"));
    }
    stderr
        .lines()
        .find_map(|line| {
            if !line.contains("instructions") {
                return None;
            }
            line.split(',').next().map(str::trim)
        })
        .ok_or_else(|| format!("instructions:u missing: {stderr}"))?
        .parse()
        .map_err(|error| format!("invalid instruction count: {error}"))
}

#[cfg(feature = "bench-reference")]
fn cv(samples: &[f64]) -> f64 {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    100.0 * variance.sqrt() / mean
}

#[cfg(feature = "bench-reference")]
fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| left.partial_cmp(right).expect("sample is not NaN"));
    samples[samples.len() / 2]
}

#[cfg(feature = "bench-reference")]
fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

#[cfg(feature = "bench-reference")]
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
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv_pct:.6}"
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
    correctness_gate();
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    let live_profile_only = env::args().any(|arg| arg == "--profile-live-only");
    run_profile(&executable, live_profile_only)
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    if live_profile_only {
        return Ok(());
    }
    #[cfg(feature = "bench-reference")]
    {
        run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
    }
    #[cfg(not(feature = "bench-reference"))]
    Err("same-binary A/B requires bench-reference".to_owned())
}
