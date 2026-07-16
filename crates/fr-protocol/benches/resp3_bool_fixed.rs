//! Profile-first, same-binary proof for fixed-width RESP3 Boolean parsing.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::{
    RespFrame, RespParseError, bench_parse_resp3_bool_candidate, bench_parse_resp3_bool_current,
    bench_parse_resp3_bool_reference,
};

const PROFILE_REPEATS: usize = 30_000_000;
const STAT_REPEATS: usize = 10_000_000;
const ROUNDS: usize = 9;
const CURRENT_SYMBOL: &str = "fr_protocol::parse_resp3_bool";
const CANDIDATE_SYMBOL: &str = "fr_protocol::bench_parse_resp3_bool_candidate";
const REFERENCE_SYMBOL: &str = "fr_protocol::bench_parse_resp3_bool_reference";

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

    const fn symbol(self) -> &'static str {
        match self {
            Self::Candidate => CANDIDATE_SYMBOL,
            Self::Reference => REFERENCE_SYMBOL,
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

fn parse_body(arm: Arm, input: &[u8], start: usize) -> Result<(RespFrame, usize), RespParseError> {
    match arm {
        Arm::Candidate => bench_parse_resp3_bool_candidate(input, start),
        Arm::Reference => bench_parse_resp3_bool_reference(input, start),
    }
}

fn run_current(repeats: usize) -> Result<(), String> {
    let mut checksum = 0_u64;
    for index in 0..repeats {
        let input: &[u8] = if index & 1 == 0 { b"#t\r\n" } else { b"#f\r\n" };
        let parsed = bench_parse_resp3_bool_current(black_box(input))
            .map_err(|error| format!("canonical Boolean failed: {error:?}"))?;
        let value = match black_box(parsed.frame) {
            RespFrame::Integer(value) => value,
            other => return Err(format!("unexpected parsed frame: {other:?}")),
        };
        checksum = checksum
            .wrapping_add(parsed.consumed as u64)
            .wrapping_add(value.unsigned_abs());
    }
    black_box(checksum);
    Ok(())
}

fn run_arm(arm: Arm, repeats: usize) {
    let mut checksum = 0_u64;
    for index in 0..repeats {
        let input: &[u8] = if index & 1 == 0 { b"#t\r\n" } else { b"#f\r\n" };
        let result = parse_body(arm, black_box(input), black_box(1));
        checksum = checksum.wrapping_add(match result.as_ref() {
            Ok((RespFrame::Integer(value), consumed)) => {
                (*consumed as u64).wrapping_add(value.unsigned_abs())
            }
            Ok(_) | Err(_) => 0,
        });
        let _ = black_box(result);
    }
    black_box(checksum);
}

fn assert_parity(input: &[u8], start: usize) {
    let candidate = parse_body(Arm::Candidate, black_box(input), black_box(start));
    let reference = parse_body(Arm::Reference, black_box(input), black_box(start));
    assert_eq!(candidate, reference, "input={input:?}, start={start}");
}

fn enumerate_bodies(body: &mut Vec<u8>, remaining: usize, cases: &mut usize) {
    const ALPHABET: [u8; 6] = [b't', b'f', b'\r', b'\n', b'x', 0xff];
    if remaining == 0 {
        let mut frame = Vec::with_capacity(body.len() + 1);
        frame.push(b'#');
        frame.extend_from_slice(body);
        assert_parity(&frame, 1);
        *cases += 1;
        return;
    }
    for byte in ALPHABET {
        body.push(byte);
        enumerate_bodies(body, remaining - 1, cases);
        body.pop();
    }
}

fn correctness_gate() {
    let focused: Vec<&[u8]> = vec![
        b"#t\r\n",
        b"#f\r\n",
        b"#t\r\n+tail\r\n",
        b"#f\r\n:7\r\n",
        b"#x\r\n",
        b"#true\r\n",
        b"#\r\n",
        b"#t",
        b"#t\r",
        b"#t\n",
        b"#t\n\r",
        b"#\xff\r\n",
    ];
    let mut cases = 0_usize;
    for input in focused {
        assert_parity(input, 1);
        cases += 1;
    }
    assert_parity(b"junk:t\r\n", 5);
    cases += 1;

    let mut overlong = Vec::with_capacity(65_540);
    overlong.push(b'#');
    overlong.extend(std::iter::repeat_n(b'x', 65_537));
    overlong.extend_from_slice(b"\r\n");
    assert_parity(&overlong, 1);
    cases += 1;

    for len in 0..=5 {
        enumerate_bodies(&mut Vec::with_capacity(len), len, &mut cases);
    }
    for (input, value) in [(&b"#t\r\n"[..], 1_i64), (&b"#f\r\n"[..], 0_i64)] {
        let parsed = bench_parse_resp3_bool_current(input).expect("production Boolean parses");
        assert_eq!(parsed.frame, RespFrame::Integer(value));
        assert_eq!(parsed.consumed, 4);
    }
    println!(
        "CORRECTNESS_GATE result=identical cases={cases} canonical_trailing_offset_invalid_overlong_exhaustive_len5=covered"
    );
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

fn binary_sha256(executable: &Path) -> Result<String, String> {
    let output = Command::new("sha256sum")
        .arg(executable)
        .output()
        .map_err(|error| format!("sha256sum launch failed: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum emitted no digest".to_owned())
}

fn exact_self_pct(report: &str, symbol: &str) -> Result<Option<f64>, String> {
    let Some(line) = report
        .lines()
        .find(|line| line.trim_end().ends_with(symbol))
    else {
        return Ok(None);
    };
    line.split_whitespace()
        .next()
        .ok_or_else(|| "missing self-time".to_owned())?
        .trim_end_matches('%')
        .parse::<f64>()
        .map(Some)
        .map_err(|error| format!("invalid self-time: {error}"))
}

fn profile(
    executable: &Path,
    label: &str,
    child_args: &[String],
    symbol: &str,
) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_protocol_bool_{}_{}_{}.data",
        process::id(),
        label,
        stamp
    ));
    let recorded = Command::new("timeout")
        .args([
            "90s",
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
        .args(child_args)
        .output()
        .map_err(|error| format!("perf record launch failed: {error}"))?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed for {label}: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let report = Command::new("timeout")
        .args(["30s", "perf", "report", "-i"])
        .arg(&data)
        .args([
            "--stdio",
            "--no-children",
            "-g",
            "none",
            "--percent-limit",
            "0.01",
        ])
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| format!("perf report launch failed: {error}"))?;
    if !report.status.success() {
        return Err(format!(
            "perf report failed for {label}: {}",
            String::from_utf8_lossy(&report.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&report.stdout);
    println!("PROFILE_TABLE_BEGIN arm={label}\n{stdout}\nPROFILE_TABLE_END arm={label}");
    let lost = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or_else(|| "perf report omitted Total Lost Samples".to_owned())?
        .rsplit(':')
        .next()
        .ok_or_else(|| "missing lost-sample count".to_owned())?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost != 0 {
        return Err(format!("{label} profile lost {lost} samples"));
    }
    let self_pct = exact_self_pct(&stdout, symbol)?
        .ok_or_else(|| format!("profile has no exact {symbol} frame"))?;
    if self_pct <= 0.0 {
        return Err(format!("{symbol} has zero self-time"));
    }
    println!("PROFILE_SELF arm={label} symbol={symbol} self_pct={self_pct:.4}");
    Ok(self_pct)
}

fn warm_child(executable: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new(executable)
        .args(args)
        .status()
        .map_err(|error| format!("warm-up launch failed: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "warm-up child failed".to_owned())
}

fn profile_current(executable: &Path) -> Result<(), String> {
    warm_child(executable, &["--child-current", "1000"])?;
    profile(
        executable,
        "current",
        &["--child-current".to_owned(), PROFILE_REPEATS.to_string()],
        CURRENT_SYMBOL,
    )?;
    Ok(())
}

fn profile_arms(executable: &Path) -> Result<(), String> {
    for arm in [Arm::Candidate, Arm::Reference] {
        warm_child(executable, &["--child", arm.name(), "1000"])?;
        profile(
            executable,
            arm.name(),
            &[
                "--child".to_owned(),
                arm.name().to_owned(),
                PROFILE_REPEATS.to_string(),
            ],
            arm.symbol(),
        )?;
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("timeout")
        .args([
            "60s",
            "perf",
            "stat",
            "--no-big-num",
            "-x,",
            "-e",
            "instructions:u",
            "--",
        ])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| format!("perf stat launch failed: {error}"))?;
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
    samples.sort_by(|left, right| left.partial_cmp(right).expect("finite ratio"));
    samples[samples.len() / 2]
}

fn run_ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut effects = Vec::with_capacity(ROUNDS);
    let mut candidate_counts = Vec::with_capacity(ROUNDS);
    let mut reference_counts = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
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
    let null_cv = cv(&nulls);
    let effect_cv = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let candidate_median = median(&mut candidate_counts);
    let reference_median = median(&mut reference_counts);
    let null_p05 = nulls[0];
    let null_p95 = nulls[nulls.len() - 1];
    let fewer = 100.0 * (1.0 - candidate_median / reference_median);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} fewer_instructions_pct={fewer:.6} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!("biased null median {null_median:.9}"));
    }
    let keep = effect_median > null_p95 && effect_median > 1.01;
    println!("DECISION keep={keep} effect={effect_median:.9} null_p95={null_p95:.9}");
    Ok(())
}

fn main() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child-current") {
        return run_current(
            args.get(2)
                .ok_or_else(|| "missing repeats".to_owned())?
                .parse()
                .map_err(|error| format!("invalid repeats: {error}"))?,
        );
    }
    if args.get(1).map(String::as_str) == Some("--child") {
        let arm = Arm::parse(args.get(2).ok_or_else(|| "missing arm".to_owned())?)?;
        let repeats = args
            .get(3)
            .ok_or_else(|| "missing repeats".to_owned())?
            .parse()
            .map_err(|error| format!("invalid repeats: {error}"))?;
        run_arm(arm, repeats);
        return Ok(());
    }

    let executable = env::current_exe().map_err(|error| format!("current_exe: {error}"))?;
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(&executable)?);
    println!("TRIGGER kind=RESP3_BOOLEAN canonical_true_false=true body_bytes=3 start=1");
    if args.iter().any(|arg| arg == "--profile-current-only") {
        profile_current(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
        return Ok(());
    }
    correctness_gate();
    profile_arms(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
