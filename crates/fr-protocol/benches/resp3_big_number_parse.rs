//! Profile-first harness for RESP3 Big Number parsing.
//!
//! The initial mode profiles the literal unchanged production parser. Once that profile proves
//! the target is live, this harness also carries the same-binary A/A+A/B proof for the one copy
//! lever.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::{
    RespFrame, RespParseError, bench_parse_resp3_big_number_candidate,
    bench_parse_resp3_big_number_current, bench_parse_resp3_big_number_reference,
};

const DIGITS: usize = 257;
const PROFILE_REPEATS: usize = 1_000_000;
const STAT_REPEATS: usize = 1_000_000;
const STAT_ROUNDS: usize = 9;
const TARGET_SYMBOL: &str = "fr_protocol::parse_frame_internal";

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
            Self::Candidate => "fr_protocol::bench_parse_resp3_big_number_candidate",
            Self::Reference => "fr_protocol::bench_parse_resp3_big_number_reference",
        }
    }
}

fn frame() -> Vec<u8> {
    let mut input = Vec::with_capacity(DIGITS + 3);
    input.push(b'(');
    input.extend(std::iter::repeat_n(b'7', DIGITS));
    input.extend_from_slice(b"\r\n");
    input
}

fn body() -> Vec<u8> {
    vec![b'7'; DIGITS]
}

fn parse_body(arm: Arm, input: &[u8]) -> Result<Vec<u8>, RespParseError> {
    match arm {
        Arm::Candidate => bench_parse_resp3_big_number_candidate(input),
        Arm::Reference => bench_parse_resp3_big_number_reference(input),
    }
}

fn run_current(repeats: usize) -> Result<(), String> {
    let input = frame();
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let parsed = bench_parse_resp3_big_number_current(black_box(&input))
            .map_err(|error| format!("canonical Big Number failed to parse: {error:?}"))?;
        let bytes = match black_box(parsed.frame) {
            RespFrame::BulkString(Some(bytes)) => bytes,
            other => return Err(format!("unexpected parsed frame: {other:?}")),
        };
        checksum = checksum
            .wrapping_add(parsed.consumed as u64)
            .wrapping_add(bytes.len() as u64);
    }
    black_box(checksum);
    Ok(())
}

fn run_arm(arm: Arm, repeats: usize) {
    let input = body();
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let result = parse_body(arm, black_box(&input));
        checksum = checksum.wrapping_add(match result.as_ref() {
            Ok(bytes) => bytes
                .len()
                .wrapping_add(usize::from(bytes[0]))
                .wrapping_add(usize::from(bytes[bytes.len() - 1])) as u64,
            Err(_) => 0,
        });
        let _ = black_box(result);
    }
    black_box(checksum);
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

fn exact_self_pct(report: &str, symbol: &str) -> Result<Option<f64>, String> {
    let Some(line) = report
        .lines()
        .find(|line| line.trim_end().ends_with(symbol))
    else {
        return Ok(None);
    };
    let pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    Ok(Some(pct))
}

fn profile_current(executable: &Path) -> Result<(), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_protocol_big_number_live_{}_{}.data",
        process::id(),
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let recorded = Command::new("timeout")
        .arg("90s")
        .arg("perf")
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
        .args(["--child-current", &PROFILE_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf record: {error}"))?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let report = Command::new("timeout")
        .arg("30s")
        .arg("perf")
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
    println!("PROFILE_TABLE_BEGIN arm=current\n{stdout}\nPROFILE_TABLE_END arm=current");
    let lost = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted Total Lost Samples")?
        .rsplit(':')
        .next()
        .ok_or("missing lost-sample count")?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost != 0 {
        return Err(format!("profile lost {lost} samples"));
    }
    let self_pct = exact_self_pct(&stdout, TARGET_SYMBOL)?
        .ok_or_else(|| format!("profile has no exact {TARGET_SYMBOL} frame"))?;
    if self_pct <= 0.0 {
        return Err(format!("{TARGET_SYMBOL} has zero self-time"));
    }
    println!("PROFILE_SELF arm=current symbol={TARGET_SYMBOL} self_pct={self_pct:.4}");
    Ok(())
}

fn correctness_gate() {
    let long = vec![b'8'; 65_536];
    let mut signed = vec![b'9'; DIGITS];
    signed.insert(0, b'-');
    let invalid_utf8 = [b'1', 0xff];
    let accepted: Vec<&[u8]> = vec![
        b"0",
        b"+1",
        b"-42",
        b"9999999999999999999999999999999999999",
        &signed,
        &long,
    ];
    let rejected: Vec<&[u8]> = vec![b"", b"+", b"-", b"12.5", b"123abc", b"1 2", &invalid_utf8];
    let mut cases = 0_usize;
    for input in accepted {
        let candidate = parse_body(Arm::Candidate, black_box(input));
        let reference = parse_body(Arm::Reference, black_box(input));
        assert_eq!(candidate, reference, "accepted body mismatch");
        assert_eq!(candidate.as_deref(), Ok(input));
        cases += 1;
    }
    for input in rejected {
        let candidate = parse_body(Arm::Candidate, black_box(input));
        let reference = parse_body(Arm::Reference, black_box(input));
        assert_eq!(candidate, reference, "rejected body mismatch");
        assert_eq!(candidate, Err(RespParseError::InvalidInteger));
        cases += 1;
    }

    let input = frame();
    let parsed = bench_parse_resp3_big_number_current(&input)
        .expect("production parser must accept the benchmark frame");
    assert_eq!(parsed.consumed, input.len());
    assert_eq!(parsed.frame, RespFrame::BulkString(Some(body())));
    println!(
        "CORRECTNESS_GATE result=identical cases={cases} signed_unsigned_long_invalid_utf8_and_production_frame=covered"
    );
}

fn profile_arm(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_protocol_big_number_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let recorded = Command::new("timeout")
        .arg("90s")
        .arg("perf")
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
            "perf record failed for {}: {}",
            arm.name(),
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let report = Command::new("timeout")
        .arg("30s")
        .arg("perf")
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
            "perf report failed for {}: {}",
            arm.name(),
            String::from_utf8_lossy(&report.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&report.stdout);
    println!(
        "PROFILE_TABLE_BEGIN arm={}\n{stdout}\nPROFILE_TABLE_END arm={}",
        arm.name(),
        arm.name()
    );
    let lost = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted Total Lost Samples")?
        .rsplit(':')
        .next()
        .ok_or("missing lost-sample count")?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost != 0 {
        return Err(format!("{} profile lost {lost} samples", arm.name()));
    }
    let symbol = arm.profile_symbol();
    let self_pct = exact_self_pct(&stdout, symbol)?
        .ok_or_else(|| format!("profile has no exact {symbol} frame"))?;
    if self_pct <= 0.0 {
        return Err(format!("{symbol} has zero self-time"));
    }
    println!(
        "PROFILE_SELF arm={} symbol={symbol} self_pct={self_pct:.4}",
        arm.name()
    );
    Ok(self_pct)
}

fn run_profiles(executable: &Path) -> Result<(), String> {
    for arm in [Arm::Candidate, Arm::Reference] {
        let warm = Command::new(executable)
            .args(["--child", arm.name(), "1000"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} profile warm-up failed", arm.name()));
        }
        profile_arm(executable, arm)?;
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("timeout")
        .arg("60s")
        .arg("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat failed for {}: {stderr}", arm.name()));
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
    let fewer_pct = 100.0 * (1.0 - candidate_median / reference_median);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} fewer_instructions_pct={fewer_pct:.6} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv_pct:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!(
            "null median exposes harness bias: {null_median:.9}"
        ));
    }
    let keep = effect_median > null_p95 && effect_median > 1.01;
    println!("DECISION keep={keep} effect={effect_median:.9} null_p95={null_p95:.9}");
    Ok(())
}

fn main() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child-current") {
        let repeats = args
            .get(2)
            .ok_or("missing current repeat count")?
            .parse()
            .map_err(|error| format!("invalid repeat count: {error}"))?;
        run_current(repeats)?;
        return Ok(());
    }

    if args.get(1).map(String::as_str) == Some("--child") {
        let arm = Arm::parse(args.get(2).ok_or("missing child arm")?)?;
        let repeats = args
            .get(3)
            .ok_or("missing arm repeat count")?
            .parse()
            .map_err(|error| format!("invalid repeat count: {error}"))?;
        run_arm(arm, repeats);
        return Ok(());
    }

    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    if args.iter().any(|arg| arg == "--profile-current-only") {
        let input = frame();
        let parsed = bench_parse_resp3_big_number_current(&input)
            .map_err(|error| format!("current parser rejected fixture: {error:?}"))?;
        assert_eq!(parsed.consumed, input.len());
        assert_eq!(parsed.frame, RespFrame::BulkString(Some(body())));
        println!("WORKER_ID {}", worker_id());
        println!("BINARY_SHA256 current={}", binary_sha256(&executable)?);
        println!("TRIGGER kind=RESP3_BIG_NUMBER digits={DIGITS} sign=none canonical=true");
        let warm = Command::new(&executable)
            .args(["--child-current", "1000"])
            .status()
            .map_err(|error| format!("could not launch profile warm-up: {error}"))?;
        if !warm.success() {
            return Err("profile warm-up failed".to_owned());
        }
        return profile_current(&executable).map_err(|error| format!("PROFILE INVALID: {error}"));
    }

    correctness_gate();
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(&executable)?);
    println!(
        "TRIGGER kind=RESP3_BIG_NUMBER_VALIDATED_BODY digits={DIGITS} sign=none canonical=true"
    );
    run_profiles(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
