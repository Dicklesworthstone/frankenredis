//! Same-binary proof for retaining replica ACK/FACK snapshot vector capacity.
//!
//! Both arms execute the full steady-state `REPLCONF ACK` runtime path with one registered replica.
//! Candidate clears and refills the two long-lived snapshot vectors. Reference retains the literal
//! prior pair of `collect::<Vec<_>>()` assignments.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::RespFrame;
use fr_runtime::{Runtime, bench_select_replica_ack_snapshot_owned_reference};

const PROFILE_REPEATS: usize = 750_000;
const PROFILE_TRIALS: usize = 3;
const STAT_REPEATS: usize = 200_000;
const STAT_ROUNDS: usize = 24;

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

    const fn uses_owned_reference(self) -> bool {
        matches!(self, Self::Reference)
    }

    const fn profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "refresh_replica_ack_snapshots_reusing_capacity",
            Self::Reference => "refresh_replica_ack_snapshots_owned_reference",
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "refresh_replica_ack_snapshots_owned_reference",
            Self::Reference => "refresh_replica_ack_snapshots_reusing_capacity",
        }
    }
}

fn argv(parts: &[&[u8]]) -> Vec<Vec<u8>> {
    parts.iter().map(|part| part.to_vec()).collect()
}

fn execute(runtime: &mut Runtime, command: &[Vec<u8>], tick: usize) -> RespFrame {
    runtime.execute_argv_with_unix_time_us(
        black_box(command),
        black_box(tick as u64 + 1),
        black_box(1_700_000_000_000_000_u64 + tick as u64),
    )
}

fn runtime_with_registered_replica(arm: Arm) -> Runtime {
    bench_select_replica_ack_snapshot_owned_reference(arm.uses_owned_reference());
    let mut runtime = Runtime::default_strict();
    let listening_port = argv(&[b"REPLCONF", b"listening-port", b"6380"]);
    assert_eq!(
        execute(&mut runtime, &listening_port, 0),
        RespFrame::SimpleString("OK".to_owned())
    );
    let ack = argv(&[b"REPLCONF", b"ACK", b"10"]);
    assert_eq!(
        execute(&mut runtime, &ack, 1),
        RespFrame::SimpleString("OK".to_owned())
    );
    runtime
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut runtime = runtime_with_registered_replica(arm);
    let ack = argv(&[b"REPLCONF", b"ACK", b"10"]);
    let mut checksum = 0_u64;
    for tick in 0..repeats {
        let reply = execute(&mut runtime, &ack, tick + 2);
        checksum = checksum.wrapping_add(match black_box(&reply) {
            RespFrame::SimpleString(value) => value.len() as u64,
            unexpected => panic!("unexpected ACK reply: {unexpected:?}"),
        });
        black_box(reply);
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

fn correctness_sequence(arm: Arm) -> Vec<RespFrame> {
    bench_select_replica_ack_snapshot_owned_reference(arm.uses_owned_reference());
    let mut runtime = Runtime::default_strict();
    [
        argv(&[b"REPLCONF", b"listening-port", b"6380"]),
        argv(&[b"REPLCONF", b"ACK", b"10"]),
        argv(&[b"REPLCONF", b"ACK", b"5"]),
        argv(&[b"REPLCONF", b"ACK", b"20", b"FACK", b"12"]),
        argv(&[b"REPLCONF", b"FACK", b"18"]),
        argv(&[b"REPLCONF", b"GETACK", b"*"]),
        argv(&[b"WAIT", b"1", b"0"]),
        argv(&[b"WAITAOF", b"0", b"1", b"0"]),
    ]
    .iter()
    .enumerate()
    .map(|(tick, command)| execute(&mut runtime, command, tick))
    .collect()
}

fn correctness_gate() {
    let candidate = correctness_sequence(Arm::Candidate);
    let reference = correctness_sequence(Arm::Reference);
    assert_eq!(candidate, reference);
    println!(
        "CORRECTNESS_GATE full_replconf_ack_fack_getack_wait_waitaof_sequence=identical replies={}",
        candidate.len()
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

fn profile_trial(executable: &Path, arm: Arm, trial: usize) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_replconf_ack_snapshot_{}_{}_{}_{}.data",
        process::id(),
        arm.name(),
        trial,
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
            "0.05",
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
        "PROFILE_TABLE_BEGIN arm={} trial={trial}\n{stdout}\nPROFILE_TABLE_END arm={} trial={trial}",
        arm.name(),
        arm.name()
    );
    let lost_line = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted Total Lost Samples; profile provenance INVALID")?;
    let lost_samples = lost_line
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

fn run_profile(executable: &Path) -> Result<(), String> {
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("WORKER_ID {hostname}");
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!("TRIGGER primary_registered_replicas=1 snapshot_capacity_warmed=true");
    for arm in [Arm::Reference, Arm::Candidate] {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
    }

    let candidate_self = profile_trial(executable, Arm::Candidate, 1)?;
    println!("PROFILE_SELF arm=candidate trial=1 self_pct={candidate_self:.4}");

    let mut reference_samples = Vec::with_capacity(PROFILE_TRIALS);
    for trial in 1..=PROFILE_TRIALS {
        let self_pct = profile_trial(executable, Arm::Reference, trial)?;
        println!("PROFILE_SELF arm=reference trial={trial} self_pct={self_pct:.4}");
        reference_samples.push(self_pct);
    }
    let reference_cv = cv(&reference_samples);
    let reference_median = median(&mut reference_samples);
    println!(
        "PROFILE_SELF_SUMMARY arm=reference trials={PROFILE_TRIALS} median_self_pct={reference_median:.4} self_cv_pct={reference_cv:.4} samples={reference_samples:?}"
    );
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
    run_profile(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
