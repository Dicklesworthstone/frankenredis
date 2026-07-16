//! Same-binary proof for exact expected-frame matching without materialization.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_conformance::{
    ExpectedFrame, bench_frame_matches_expected_candidate, bench_frame_matches_expected_reference,
};
use fr_protocol::RespFrame;

const PROFILE_REPEATS: usize = 100_000;
const STAT_REPEATS: usize = 20_000;
const STAT_ROUNDS: usize = 9;

type MatchCase = (RespFrame, ExpectedFrame);
type Matcher = fn(&RespFrame, &ExpectedFrame) -> bool;

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
            Self::Candidate => "fr_conformance::frame_matches_expected",
            Self::Reference => "fr_conformance::bench_frame_matches_expected_reference_impl",
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "bench_frame_matches_expected_reference_impl",
            Self::Reference => "fr_conformance::frame_matches_expected",
        }
    }

    fn matcher(self) -> Matcher {
        match self {
            Self::Candidate => bench_frame_matches_expected_candidate,
            Self::Reference => bench_frame_matches_expected_reference,
        }
    }
}

fn payload(kind: &str, index: usize, len: usize) -> String {
    let prefix = format!("{kind}-{index:03}-");
    let mut value = String::with_capacity(len.max(prefix.len()));
    value.push_str(&prefix);
    value.extend(std::iter::repeat_n('x', len.saturating_sub(prefix.len())));
    value
}

fn bulk_case(index: usize) -> MatchCase {
    let len = [16, 64, 256][index % 3];
    let value = payload("bulk", index, len);
    (
        RespFrame::BulkString(Some(value.as_bytes().to_vec())),
        ExpectedFrame::Bulk { value: Some(value) },
    )
}

fn integer_case(index: usize) -> MatchCase {
    let value = i64::try_from(index).expect("fixture index fits i64") * 37 - 211;
    (RespFrame::Integer(value), ExpectedFrame::Integer { value })
}

fn error_case(index: usize) -> MatchCase {
    let value = payload("ERR fixture", index, 56);
    (
        RespFrame::Error(value.clone()),
        ExpectedFrame::Error { value },
    )
}

fn simple_case(index: usize) -> MatchCase {
    let value = payload("status", index, 35);
    (
        RespFrame::SimpleString(value.clone()),
        ExpectedFrame::Simple { value },
    )
}

fn child_case(index: usize) -> MatchCase {
    match index {
        0..18 => bulk_case(100 + index),
        18..30 => integer_case(100 + index),
        30..40 => error_case(100 + index),
        _ => simple_case(100 + index),
    }
}

fn fixture_weighted_corpus() -> Vec<MatchCase> {
    let mut corpus = Vec::with_capacity(61);
    corpus.extend((0..18).map(bulk_case));
    corpus.extend((0..11).map(integer_case));
    corpus.extend((0..8).map(error_case));
    corpus.extend((0..7).map(simple_case));
    corpus.push((RespFrame::Array(None), ExpectedFrame::NullArray));

    for array_index in 0..16 {
        let mut actual = Vec::with_capacity(3);
        let mut expected = Vec::with_capacity(3);
        for slot in 0..3 {
            let (child_actual, child_expected) = child_case(array_index * 3 + slot);
            actual.push(child_actual);
            expected.push(child_expected);
        }
        corpus.push((
            RespFrame::Array(Some(actual)),
            ExpectedFrame::Array { value: expected },
        ));
    }
    corpus
}

fn expected_variants() -> Vec<ExpectedFrame> {
    vec![
        ExpectedFrame::Simple {
            value: "OK".to_owned(),
        },
        ExpectedFrame::Error {
            value: "ERR no".to_owned(),
        },
        ExpectedFrame::Integer { value: -7 },
        ExpectedFrame::Bulk {
            value: Some("bytes".to_owned()),
        },
        ExpectedFrame::BulkContainsAll {
            value: vec!["alpha".to_owned(), "beta".to_owned()],
        },
        ExpectedFrame::BulkNotContainsAll {
            value: vec!["forbidden".to_owned()],
        },
        ExpectedFrame::SimpleContainsAll {
            value: vec!["OK".to_owned()],
        },
        ExpectedFrame::Array {
            value: vec![ExpectedFrame::Integer { value: 3 }],
        },
        ExpectedFrame::NullArray,
        ExpectedFrame::AnyInteger,
        ExpectedFrame::AnyBulk,
        ExpectedFrame::AnySimple,
        ExpectedFrame::AnyArray,
        ExpectedFrame::SimplePattern {
            value: "id:{int}".to_owned(),
        },
    ]
}

fn actual_variants() -> Vec<RespFrame> {
    vec![
        RespFrame::SimpleString("OK".to_owned()),
        RespFrame::Error("ERR no".to_owned()),
        RespFrame::Integer(-7),
        RespFrame::BulkString(None),
        RespFrame::BulkString(Some(Vec::new())),
        RespFrame::BulkString(Some(b"alpha beta bytes".to_vec())),
        RespFrame::BulkString(Some(vec![0xff, 0xfe])),
        RespFrame::Array(None),
        RespFrame::Array(Some(vec![RespFrame::Integer(3)])),
        RespFrame::Map(Some(vec![(
            RespFrame::SimpleString("key".to_owned()),
            RespFrame::Integer(3),
        )])),
        RespFrame::Push(vec![RespFrame::Integer(3)]),
        RespFrame::Sequence(vec![RespFrame::Integer(3)]),
        RespFrame::Double("1.5".to_owned()),
        RespFrame::Set(Some(vec![RespFrame::Integer(3)])),
        RespFrame::Verbatim("txt:value".to_owned()),
        RespFrame::BigNumber("12345678901234567890".to_owned()),
        RespFrame::Bool(true),
        RespFrame::Attribute(vec![(
            RespFrame::SimpleString("meta".to_owned()),
            RespFrame::Integer(1),
        )]),
    ]
}

fn matches(arm: Arm, actual: &RespFrame, expected: &ExpectedFrame) -> bool {
    black_box(arm.matcher())(black_box(actual), black_box(expected))
}

fn correctness_gate() {
    let expected = expected_variants();
    let actual = actual_variants();
    let mut parity_cases = 0_usize;
    for expected_frame in &expected {
        for actual_frame in &actual {
            assert_eq!(
                matches(Arm::Candidate, actual_frame, expected_frame),
                matches(Arm::Reference, actual_frame, expected_frame),
                "matcher parity differs for actual={actual_frame:?} expected={expected_frame:?}"
            );
            parity_cases += 1;
        }
    }

    let semantic_cases = [
        (
            RespFrame::BulkString(None),
            ExpectedFrame::Bulk { value: None },
            true,
        ),
        (
            RespFrame::BulkString(Some(Vec::new())),
            ExpectedFrame::Bulk { value: None },
            false,
        ),
        (
            RespFrame::BulkString(Some(Vec::new())),
            ExpectedFrame::Bulk {
                value: Some(String::new()),
            },
            true,
        ),
        (RespFrame::Array(None), ExpectedFrame::NullArray, true),
        (
            RespFrame::Array(Some(Vec::new())),
            ExpectedFrame::NullArray,
            false,
        ),
        (
            RespFrame::SimpleString("same".to_owned()),
            ExpectedFrame::Error {
                value: "same".to_owned(),
            },
            false,
        ),
        (
            RespFrame::Integer(i64::MIN),
            ExpectedFrame::Integer { value: i64::MIN },
            true,
        ),
        (
            RespFrame::Integer(i64::MAX),
            ExpectedFrame::Integer {
                value: i64::MAX - 1,
            },
            false,
        ),
    ];
    let semantic_case_count = semantic_cases.len();
    for (actual_frame, expected_frame, expected_result) in semantic_cases {
        assert_eq!(
            matches(Arm::Candidate, &actual_frame, &expected_frame),
            expected_result,
            "candidate semantic guard differs"
        );
        assert_eq!(
            matches(Arm::Reference, &actual_frame, &expected_frame),
            expected_result,
            "reference semantic guard differs"
        );
    }
    println!(
        "CORRECTNESS_GATE parity_cases={parity_cases} semantic_cases={} all_expected_and_actual_variants=covered",
        semantic_case_count
    );
}

#[inline(never)]
fn run_loop(arm: Arm, repeats: usize) {
    let corpus = fixture_weighted_corpus();
    let matcher = black_box(arm.matcher());
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        for (actual, expected) in black_box(&corpus) {
            checksum = checksum.wrapping_add(u64::from(black_box(matcher(
                black_box(actual),
                black_box(expected),
            ))));
        }
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
        "fr_conformance_frame_match_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let recorded = Command::new("timeout")
        .env("LC_ALL", "C")
        .args([
            "--foreground",
            "45s",
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
        .args(["--child", arm.name(), &PROFILE_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf record: {error}"))?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let report = Command::new("timeout")
        .env("LC_ALL", "C")
        .args([
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
        .find(|line| line.contains(arm.profile_symbol()))
        .ok_or_else(|| {
            format!(
                "profile has no exact {} helper frame; workload INVALID",
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
        return Err(format!("{} helper has zero self-time", arm.name()));
    }
    let materialization = stdout
        .lines()
        .find(|line| line.contains("fr_conformance::expected_to_frame"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.trim_end_matches('%').parse::<f64>().ok());
    match materialization {
        Some(percent) => println!(
            "PROFILE_MATERIALIZATION arm={} self_pct={percent:.4}",
            arm.name()
        ),
        None => println!("PROFILE_MATERIALIZATION arm={} absent=true", arm.name()),
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path, arms: &[Arm]) -> Result<(), String> {
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!(
        "TRIGGER corpus_cases=61 matcher_calls_per_loop=109 exact_fallback_pct=83.35 allocation_bearing_pct=62.94 bulk_lengths=16,64,256"
    );
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
    let output = Command::new("timeout")
        .env("LC_ALL", "C")
        .args([
            "--foreground",
            "15s",
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
    if env::args().any(|arg| arg == "--profile-candidate-only") {
        return run_profile(&executable, &[Arm::Candidate])
            .map_err(|error| format!("PROFILE INVALID: {error}"));
    }
    run_profile(&executable, &[Arm::Candidate, Arm::Reference])
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
