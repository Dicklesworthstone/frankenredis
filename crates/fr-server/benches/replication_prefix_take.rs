//! Same-binary proof for extracting complete RESP frames from a replica read buffer.
//!
//! The frozen reference copies the complete prefix and then drains it. The candidate may move the
//! complete prefix out only if it leaves the exact incomplete tail and error behavior unchanged.

use std::{
    env,
    hint::black_box,
    io::ErrorKind,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::ParserConfig;
use fr_server::{
    bench_consume_complete_replication_prefix_reference, consume_complete_replication_prefix,
};

const FRAME_COUNT: usize = 16;
const VALUE_LEN: usize = 4_096;
// Collect enough samples to retain the candidate's small amount of caller-owned work. The hot
// parser and libc copies otherwise consume nearly every sample in a sub-100 ms profile.
const PROFILE_REPEATS: usize = 1_200_000;
const STAT_REPEATS: usize = 2_000;
const STAT_ROUNDS: usize = 9;
const PARTIAL_TAIL: &[u8] = b"*3\r\n$3\r\nSET\r\n$4\r\ntail\r\n$8\r\npar";

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
            Self::Candidate => "fr_server::consume_complete_replication_prefix",
            Self::Reference => "fr_server::bench_consume_complete_replication_prefix_reference",
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "bench_consume_complete_replication_prefix_reference",
            Self::Reference => "fr_server::consume_complete_replication_prefix",
        }
    }
}

fn build_trigger() -> (Vec<u8>, usize) {
    let mut input = Vec::with_capacity(FRAME_COUNT * (VALUE_LEN + 64) + PARTIAL_TAIL.len());
    for index in 0..FRAME_COUNT {
        input.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$8\r\n");
        input.extend_from_slice(format!("key{index:05}").as_bytes());
        input.extend_from_slice(b"\r\n$4096\r\n");
        input.resize(input.len() + VALUE_LEN, b'a' + (index % 26) as u8);
        input.extend_from_slice(b"\r\n");
    }
    let complete_len = input.len();
    input.extend_from_slice(PARTIAL_TAIL);
    (input, complete_len)
}

fn consume(input: &mut Vec<u8>, config: &ParserConfig, arm: Arm) -> std::io::Result<Vec<u8>> {
    match arm {
        Arm::Candidate => consume_complete_replication_prefix(input, config),
        Arm::Reference => bench_consume_complete_replication_prefix_reference(input, config),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let (seed, _) = build_trigger();
    let config = ParserConfig::default();
    let mut checksum = 0_u64;
    let mut input = Vec::new();
    for _ in 0..repeats {
        input.clear();
        input.extend_from_slice(black_box(&seed));
        let outcome = consume(black_box(&mut input), black_box(&config), arm);
        match black_box(&outcome) {
            Ok(payload) => {
                checksum = checksum
                    .wrapping_add(payload.len() as u64)
                    .wrapping_add(input.len() as u64)
                    .wrapping_add(u64::from(payload.first().copied().unwrap_or(0)))
                    .wrapping_add(u64::from(payload.last().copied().unwrap_or(0)))
                    .wrapping_add(u64::from(input.first().copied().unwrap_or(0)));
            }
            Err(error) => {
                checksum = checksum.wrapping_add(error.raw_os_error().unwrap_or(1) as u64)
            }
        }
        let _ = black_box(outcome);
        let _ = black_box(&input);
    }
    black_box(checksum);
}

type ComparableOutcome = (Result<Vec<u8>, (ErrorKind, String)>, Vec<u8>);

fn comparable_outcome(input: &[u8], arm: Arm) -> ComparableOutcome {
    let mut read_buf = input.to_vec();
    let result = consume(&mut read_buf, &ParserConfig::default(), arm)
        .map_err(|error| (error.kind(), error.to_string()));
    (result, read_buf)
}

fn correctness_gate() {
    let mut two_frames = b"*1\r\n$4\r\nPING\r\n".to_vec();
    two_frames.extend_from_slice(b"*2\r\n$4\r\nECHO\r\n$2\r\nhi\r\n");
    let mut complete_plus_partial = two_frames.clone();
    complete_plus_partial.extend_from_slice(PARTIAL_TAIL);
    let mut binary_bulk = b"*1\r\n$4\r\n".to_vec();
    binary_bulk.extend_from_slice(&[0, 0xff, b'x', b'y']);
    binary_bulk.extend_from_slice(b"\r\n");
    let (trigger, complete_len) = build_trigger();
    let cases = [
        Vec::new(),
        b"*1\r\n$4\r\nPING\r\n".to_vec(),
        two_frames,
        complete_plus_partial,
        PARTIAL_TAIL.to_vec(),
        b"?bad\r\n".to_vec(),
        b"$-1\r\n".to_vec(),
        binary_bulk,
        trigger.clone(),
    ];
    for (index, input) in cases.iter().enumerate() {
        assert_eq!(
            comparable_outcome(input, Arm::Candidate),
            comparable_outcome(input, Arm::Reference),
            "replica prefix extraction differs for case {index}"
        );
    }
    let (payload, tail) = comparable_outcome(&trigger, Arm::Candidate);
    assert_eq!(payload.expect("trigger is valid").len(), complete_len);
    assert_eq!(tail, PARTIAL_TAIL);
    println!(
        "CORRECTNESS_GATE parser=identical cases={} complete_prefix_and_partial_tail=covered",
        cases.len()
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
        "fr_server_replication_prefix_{}_{}_{}.data",
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
    let (trigger, complete_len) = build_trigger();
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!(
        "TRIGGER bytes={} complete_bytes={complete_len} partial_tail_bytes={} frames={FRAME_COUNT} value_bytes={VALUE_LEN}",
        trigger.len(),
        PARTIAL_TAIL.len()
    );
    for &arm in arms {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "10"])
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
