//! Same-binary A/A+A/B proof for TLS protocol duplicate suppression.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_config::{
    TlsCfgError, TlsProtocol, bench_parse_tls_protocols_reference, parse_tls_protocols,
};

const INPUT: &str = " TLSv1 TLSv1.1 TLSv1.2 TLSv1.3 tlsV1 TLSV1.1 tlsv1.2 tlsv1.3 TLSv1.3 TLSv1.2 TLSv1.1 TLSv1 TLSv1 TLSv1.1 TLSv1.2 TLSv1.3 ";
const PROFILE_REPEATS: usize = 500_000;
const STAT_REPEATS: usize = 220_000;
const STAT_ROUNDS: usize = 9;

type ParseResult = Result<Vec<TlsProtocol>, TlsCfgError>;

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
            Self::Candidate => "fr_config::parse_tls_protocols",
            Self::Reference => "fr_config::bench_parse_tls_protocols_reference",
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "bench_parse_tls_protocols_reference",
            Self::Reference => "fr_config::parse_tls_protocols",
        }
    }
}

fn parse(raw: &str, arm: Arm) -> ParseResult {
    match arm {
        Arm::Candidate => parse_tls_protocols(black_box(raw)),
        Arm::Reference => bench_parse_tls_protocols_reference(black_box(raw)),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let parsed = parse(black_box(INPUT), arm);
        match black_box(&parsed) {
            Ok(protocols) => {
                checksum = checksum.wrapping_add(protocols.len() as u64);
                for protocol in protocols {
                    checksum = checksum.wrapping_add(protocol.as_token().len() as u64);
                }
            }
            Err(error) => checksum = checksum.wrapping_add(error.reason_code().len() as u64),
        }
        let _ = black_box(parsed);
    }
    black_box(checksum);
}

fn correctness_gate() {
    let cases = [
        "",
        "TLSv1",
        "TLSv1 TLSv1.1 TLSv1.2 TLSv1.3",
        "TLSV1 tlsv1 TLSv1",
        " TLSv1.3  TLSv1.2 TLSv1.3 ",
        "   ",
        "TLSv1.4",
        "TLSv1,TLSv1.2",
        INPUT,
    ];
    for (index, raw) in cases.iter().enumerate() {
        assert_eq!(
            parse(raw, Arm::Candidate),
            parse(raw, Arm::Reference),
            "TLS protocol parser differs for case {index}: {raw:?}"
        );
    }
    println!(
        "CORRECTNESS_GATE result=identical cases={} empty_duplicate_case_whitespace_invalid=covered",
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

fn profile(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_config_tls_protocols_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
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
        .args(["report", "-i"])
        .arg(&data)
        .args([
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
    let lost = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted lost-sample count")?
        .rsplit(':')
        .next()
        .ok_or("missing lost-sample count")?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost != 0 {
        return Err(format!("profile lost {lost} samples"));
    }
    if stdout
        .lines()
        .any(|line| line.trim_end().ends_with(arm.wrong_profile_symbol()))
    {
        return Err(format!("{} profile executed the wrong arm", arm.name()));
    }
    let line = stdout
        .lines()
        .find(|line| line.contains(arm.profile_symbol()))
        .ok_or_else(|| format!("profile has no {} frame", arm.profile_symbol()))?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err("profile helper has zero self-time".to_owned());
    }
    println!(
        "PROFILE_SELF arm={} self_pct={self_pct:.4} lost_samples={lost}",
        arm.name()
    );
    Ok(self_pct)
}

fn instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "perf stat failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split(',').collect();
            fields
                .iter()
                .any(|field| field.contains("instructions"))
                .then(|| fields[0].trim())
        })
        .ok_or_else(|| "perf stat emitted no instructions".to_owned())?
        .parse()
        .map_err(|error| format!("invalid instruction count: {error}"))
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|left, right| left.partial_cmp(right).expect("ratios are finite"));
    values[values.len() / 2]
}

fn cv(values: &[f64]) -> f64 {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64;
    100.0 * variance.sqrt() / mean
}

fn ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    let mut candidates = Vec::with_capacity(STAT_ROUNDS);
    let mut references = Vec::with_capacity(STAT_ROUNDS);
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
            counts[slot] = instructions(executable, arm)?;
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
        candidates.push(counts[0] as f64);
        references.push(counts[2] as f64);
    }
    let null_cv = cv(&nulls);
    let effect_cv = cv(&effects);
    let mut null_sorted = nulls.clone();
    null_sorted.sort_by(|left, right| left.partial_cmp(right).expect("ratios are finite"));
    let null_low = null_sorted[0];
    let null_high = null_sorted[null_sorted.len() - 1];
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let candidate_median = median(&mut candidates);
    let reference_median = median(&mut references);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} fewer_instructions_pct={:.6} null_median={null_median:.9} null_spread={null_low:.9}..{null_high:.9} null_cv_pct={null_cv:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv:.6}",
        100.0 * (1.0 - candidate_median / reference_median)
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!("null-control gate failed: {null_median:.9}"));
    }
    if effect_median <= null_high || effect_median <= 1.01 {
        return Err(format!(
            "keep gate failed effect={effect_median:.9} null_high={null_high:.9}"
        ));
    }
    println!("DECISION keep=true effect={effect_median:.9}");
    Ok(())
}

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = child_args()? {
        run_loop(arm, repeats);
        process::exit(0);
    }
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    correctness_gate();
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(&executable)?);
    println!(
        "TRIGGER bytes={} tokens=16 unique=4 duplicates=12",
        INPUT.len()
    );
    profile(&executable, Arm::Candidate)
        .map_err(|error| format!("PROFILE INVALID candidate: {error}"))?;
    profile(&executable, Arm::Reference)
        .map_err(|error| format!("PROFILE INVALID reference: {error}"))?;
    ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
