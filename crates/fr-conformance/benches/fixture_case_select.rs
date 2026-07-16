//! Profile-first, same-binary proof for live-oracle fixture case selection.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_conformance::{
    ConformanceCase, ConformanceFixture, bench_select_conformance_fixture_cases_candidate,
    bench_select_conformance_fixture_cases_current,
    bench_select_conformance_fixture_cases_reference,
};

const PROFILE_REPEATS: usize = 1_000;
const STAT_REPEATS: usize = 100;
const ROUNDS: usize = 9;
const CURRENT_SYMBOL: &str = "fr_conformance::select_conformance_fixture_cases";
const CANDIDATE_SYMBOL: &str = "fr_conformance::bench_select_conformance_fixture_cases_candidate";
const REFERENCE_SYMBOL: &str = "fr_conformance::bench_select_conformance_fixture_cases_reference";

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

fn select(
    arm: Arm,
    fixture: &ConformanceFixture,
    case_names: &[&str],
) -> Result<ConformanceFixture, String> {
    match arm {
        Arm::Candidate => bench_select_conformance_fixture_cases_candidate(fixture, case_names),
        Arm::Reference => bench_select_conformance_fixture_cases_reference(fixture, case_names),
    }
}

fn real_substrate() -> Result<(ConformanceFixture, Vec<String>), String> {
    let fixture: ConformanceFixture =
        serde_json::from_str(include_str!("../fixtures/core_zset.json"))
            .map_err(|error| format!("core_zset fixture: {error}"))?;
    let names: Vec<String> = fixture.cases.iter().map(|case| case.name.clone()).collect();
    if fixture.cases.len() != 324 || names.len() != 324 {
        return Err(format!(
            "unexpected real substrate: fixture={} requested={}",
            fixture.cases.len(),
            names.len()
        ));
    }
    Ok((fixture, names))
}

fn consume_result(result: Result<ConformanceFixture, String>, checksum: &mut u64) {
    match &result {
        Ok(fixture) => {
            *checksum = checksum
                .wrapping_add(fixture.cases.len() as u64)
                .wrapping_add(fixture.suite.len() as u64)
                .wrapping_add(fixture.cases.first().map_or(0, |case| case.now_ms));
        }
        Err(error) => *checksum = checksum.wrapping_add(error.len() as u64),
    }
    let _ = black_box(result);
}

fn run_arm(arm: Arm, repeats: usize) -> Result<(), String> {
    let (fixture, names) = real_substrate()?;
    let case_names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let result = select(arm, black_box(&fixture), black_box(&case_names));
        consume_result(result, &mut checksum);
    }
    black_box(checksum);
    Ok(())
}

fn run_current(repeats: usize) -> Result<(), String> {
    let (fixture, names) = real_substrate()?;
    let case_names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let result = bench_select_conformance_fixture_cases_current(
            black_box(&fixture),
            black_box(&case_names),
        );
        consume_result(result, &mut checksum);
    }
    black_box(checksum);
    Ok(())
}

fn result_bytes(result: Result<ConformanceFixture, String>) -> Result<Vec<u8>, String> {
    match result {
        Ok(fixture) => serde_json::to_vec(&fixture).map_err(|error| error.to_string()),
        Err(error) => Ok(error.into_bytes()),
    }
}

fn assert_parity(fixture: &ConformanceFixture, case_names: &[&str]) {
    let candidate = result_bytes(select(Arm::Candidate, fixture, case_names));
    let reference = result_bytes(select(Arm::Reference, fixture, case_names));
    assert_eq!(candidate, reference, "case_names={case_names:?}");
}

fn correctness_gate() -> Result<(), String> {
    let (fixture, names) = real_substrate()?;
    let requested: Vec<&str> = names.iter().map(String::as_str).collect();
    let first_41 = &requested[..41];
    let mut reversed = requested.clone();
    reversed.reverse();
    let duplicate_request = [requested[0], requested[0], requested[17], requested[0]];
    let missing = [requested[0], "missing-z", "missing-a", requested[1]];

    assert_parity(&fixture, &[]);
    assert_parity(&fixture, &[requested[0]]);
    assert_parity(&fixture, &requested);
    assert_parity(&fixture, first_41);
    assert_parity(&fixture, &reversed);
    assert_parity(&fixture, &duplicate_request);
    assert_parity(&fixture, &missing);

    let first = fixture.cases[0].clone();
    let mut second = first.clone();
    second.now_ms = first.now_ms.wrapping_add(1);
    second.argv.push("duplicate-fixture-sentinel".to_owned());
    let duplicate_fixture = ConformanceFixture {
        suite: "duplicate_fixture_names".to_owned(),
        cases: vec![first.clone(), second],
    };
    assert_parity(&duplicate_fixture, &[first.name.as_str()]);
    let selected = select(Arm::Candidate, &duplicate_fixture, &[first.name.as_str()])?;
    assert_eq!(selected.cases[0].now_ms, first.now_ms);
    assert_eq!(selected.cases[0].argv, first.argv);

    let empty_fixture = ConformanceFixture {
        suite: "empty".to_owned(),
        cases: Vec::<ConformanceCase>::new(),
    };
    assert_parity(&empty_fixture, &[]);
    assert_parity(&empty_fixture, &["missing"]);

    println!(
        "CORRECTNESS_GATE result=identical cases=10 real_fixture=324 real_requested=324 order_duplicates_first_match_missing_error_empty=covered"
    );
    Ok(())
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
        "fr_conformance_case_select_{}_{}_{}.data",
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
    warm_child(executable, &["--child-current", "20"])?;
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
        warm_child(executable, &["--child", arm.name(), "20"])?;
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
        return run_arm(arm, repeats);
    }

    let executable = env::current_exe().map_err(|error| format!("current_exe: {error}"))?;
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(&executable)?);
    println!("TRIGGER fixture=core_zset total=324 requested=324 source=live_oracle_test_order");
    if args.iter().any(|arg| arg == "--profile-current-only") {
        profile_current(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
        return Ok(());
    }
    correctness_gate()?;
    profile_arms(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
