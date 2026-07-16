//! Same-binary proof for RESP3 scalar-leaf dispatch.
//!
//! The frozen reference routes every non-null scalar through `RespFrame::encode_into`, paying a
//! second enum match. The candidate emits those identical scalar bytes directly while retaining
//! the recursive RESP3 null-promotion rules for every container.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::{RespFrame, bench_encode_into_resp3_reference};

const MAP_PAIRS: usize = 128;
const PROFILE_REPEATS: usize = 200_000;
const STAT_REPEATS: usize = 75_000;
const STAT_ROUNDS: usize = 12;

const FALLBACK_SYMBOL: &str = "<fr_protocol::RespFrame>::encode_into";
const INDIRECT_IMPL_SYMBOL: &str = "<fr_protocol::RespFrame>::encode_into_resp3_impl::<false>";
const DIRECT_IMPL_SYMBOL: &str = "<fr_protocol::RespFrame>::encode_into_resp3_impl::<true>";

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

    const fn impl_symbol(self, candidate_direct: bool) -> &'static str {
        match (self, candidate_direct) {
            (Self::Candidate, true) => DIRECT_IMPL_SYMBOL,
            (Self::Candidate | Self::Reference, false) | (Self::Reference, true) => {
                INDIRECT_IMPL_SYMBOL
            }
        }
    }
}

fn bulk(bytes: impl Into<Vec<u8>>) -> RespFrame {
    RespFrame::BulkString(Some(bytes.into()))
}

fn config_map(pair_count: usize) -> RespFrame {
    let entries = (0..pair_count)
        .map(|index| {
            (
                bulk(format!("config-key-{index:03}").into_bytes()),
                bulk(format!("value-{index:03}").into_bytes()),
            )
        })
        .collect();
    RespFrame::Map(Some(entries))
}

fn encode(frame: &RespFrame, arm: Arm, out: &mut Vec<u8>) {
    match arm {
        Arm::Candidate => frame.encode_into_resp3(out),
        Arm::Reference => bench_encode_into_resp3_reference(frame, out),
    }
}

fn correctness_gate() {
    let cases = vec![
        RespFrame::SimpleString("OK".to_owned()),
        RespFrame::SimpleString("dirty\rline\nbody".to_owned()),
        RespFrame::Error("ERR bad\r\ninput".to_owned()),
        RespFrame::Integer(i64::MIN),
        RespFrame::Integer(i64::MAX),
        bulk(Vec::new()),
        bulk(vec![0, b'\r', 0xff, b'\n']),
        RespFrame::BulkString(None),
        RespFrame::Array(None),
        RespFrame::Map(None),
        RespFrame::Set(None),
        RespFrame::Double("-1.25e+30".to_owned()),
        RespFrame::BigNumber("3492890328409238509324850943850943825024385".to_owned()),
        RespFrame::Bool(true),
        RespFrame::Bool(false),
        RespFrame::Verbatim("server-info\r\nline-two".to_owned()),
        RespFrame::Array(Some(vec![
            bulk(b"hit".to_vec()),
            RespFrame::BulkString(None),
        ])),
        RespFrame::Map(Some(vec![(
            bulk(b"key".to_vec()),
            RespFrame::Array(Some(vec![RespFrame::Integer(7), RespFrame::Array(None)])),
        )])),
        RespFrame::Set(Some(vec![bulk(b"member".to_vec()), RespFrame::Bool(true)])),
        RespFrame::Attribute(vec![(
            bulk(b"meta".to_vec()),
            RespFrame::Map(Some(vec![(bulk(b"nested".to_vec()), RespFrame::Map(None))])),
        )]),
        RespFrame::Push(vec![bulk(b"invalidate".to_vec()), RespFrame::Array(None)]),
        RespFrame::Sequence(vec![
            RespFrame::Attribute(vec![(bulk(b"ttl".to_vec()), RespFrame::Integer(5))]),
            RespFrame::SimpleString("OK".to_owned()),
        ]),
        config_map(8),
    ];
    for (index, frame) in cases.iter().enumerate() {
        let mut candidate = Vec::new();
        let mut reference = Vec::new();
        encode(black_box(frame), Arm::Candidate, &mut candidate);
        encode(black_box(frame), Arm::Reference, &mut reference);
        assert_eq!(
            candidate, reference,
            "arm mismatch for case {index}: {frame:?}"
        );
    }
    println!(
        "CORRECTNESS_GATE result=identical cases=23 scalars_nulls_nested_containers_dirty_inline=covered"
    );
}

fn run_loop(arm: Arm, repeats: usize) {
    let frame = config_map(MAP_PAIRS);
    let mut out = Vec::with_capacity(8192);
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        out.clear();
        encode(black_box(&frame), arm, black_box(&mut out));
        checksum = checksum
            .wrapping_add(out.len() as u64)
            .wrapping_add(u64::from(out[0]))
            .wrapping_add(u64::from(out[out.len() / 2]))
            .wrapping_add(u64::from(out[out.len() - 1]));
        black_box(out.as_slice());
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
        .ok_or("missing child repeats")?
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

fn exact_self_pct(report: &str, symbol: &str) -> Result<Option<f64>, String> {
    let Some(line) = report
        .lines()
        .find(|line| line.trim_end().ends_with(symbol))
    else {
        return Ok(None);
    };
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    Ok(Some(self_pct))
}

fn profile_trial(executable: &Path, arm: Arm, candidate_direct: bool) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_protocol_resp3_scalar_{}_{}_{}.data",
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
    let expected_impl = arm.impl_symbol(candidate_direct);
    let wrong_impl = if expected_impl == DIRECT_IMPL_SYMBOL {
        INDIRECT_IMPL_SYMBOL
    } else {
        DIRECT_IMPL_SYMBOL
    };
    if exact_self_pct(&stdout, wrong_impl)?.is_some() {
        return Err(format!(
            "{} profile executed wrong implementation {wrong_impl}",
            arm.name(),
        ));
    }
    let self_pct = exact_self_pct(&stdout, expected_impl)?.ok_or_else(|| {
        format!(
            "profile has no exact {} implementation {expected_impl}; workload INVALID",
            arm.name()
        )
    })?;
    if self_pct <= 0.0 {
        return Err(format!("{} implementation has zero self-time", arm.name()));
    }
    let fallback_self_pct = exact_self_pct(&stdout, FALLBACK_SYMBOL)?;
    if expected_impl == INDIRECT_IMPL_SYMBOL {
        let fallback_self_pct = fallback_self_pct.ok_or_else(|| {
            format!(
                "{} indirect implementation did not execute exact fallback {FALLBACK_SYMBOL}",
                arm.name()
            )
        })?;
        if fallback_self_pct <= 0.0 {
            return Err(format!("{} fallback has zero self-time", arm.name()));
        }
        println!(
            "PROFILE_FALLBACK arm={} symbol={FALLBACK_SYMBOL} self_pct={fallback_self_pct:.4}",
            arm.name()
        );
    } else if fallback_self_pct.is_some() {
        return Err(format!(
            "{} direct implementation unexpectedly executed fallback {FALLBACK_SYMBOL}",
            arm.name()
        ));
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path, arms: &[Arm], candidate_direct: bool) -> Result<(), String> {
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!(
        "TRIGGER kind=RESP3_CONFIG_MAP pairs={MAP_PAIRS} scalar_leaves={} nulls=0 protocol=3",
        MAP_PAIRS * 2
    );
    for &arm in arms {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "100"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
        let self_pct = profile_trial(executable, arm, candidate_direct)?;
        println!(
            "PROFILE_SELF arm={} symbol={} self_pct={self_pct:.4}",
            arm.name(),
            arm.impl_symbol(candidate_direct)
        );
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
    let current_profile_only = env::args().any(|arg| arg == "--profile-current-only");
    if current_profile_only {
        return run_profile(&executable, &[Arm::Candidate], false)
            .map_err(|error| format!("PROFILE INVALID: {error}"));
    }
    run_profile(&executable, &[Arm::Candidate, Arm::Reference], true)
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
