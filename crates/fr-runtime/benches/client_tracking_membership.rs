//! Same-binary proof for maintaining the CLIENT TRACKING BCAST index only at mutations.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

const PROFILE_REPEATS: usize = 5_000_000;
const STAT_REPEATS: usize = 2_000_000;
const STAT_ROUNDS: usize = 9;

#[derive(Clone, Copy)]
enum Arm {
    Candidate,
    Reference,
}

#[derive(Clone, Copy)]
enum Lever {
    Membership,
    SnapshotReuse,
    TransactionReuse,
    TrackingReuse,
    LastCommandReuse,
    TransactionPristine,
    StableMetadata,
    ClientIdCopy,
}

impl Lever {
    const fn name(self) -> &'static str {
        match self {
            Self::Membership => "membership",
            Self::SnapshotReuse => "snapshot-reuse",
            Self::TransactionReuse => "transaction-reuse",
            Self::TrackingReuse => "tracking-reuse",
            Self::LastCommandReuse => "last-command-reuse",
            Self::TransactionPristine => "transaction-pristine",
            Self::StableMetadata => "stable-metadata",
            Self::ClientIdCopy => "client-id-copy",
        }
    }

    fn from_env() -> Result<Self, String> {
        match env::var("FR_BENCH_LEVER").as_deref() {
            Ok("snapshot-reuse") => Ok(Self::SnapshotReuse),
            Ok("transaction-reuse") => Ok(Self::TransactionReuse),
            Ok("tracking-reuse") => Ok(Self::TrackingReuse),
            Ok("last-command-reuse") => Ok(Self::LastCommandReuse),
            Ok("transaction-pristine") => Ok(Self::TransactionPristine),
            Ok("stable-metadata") => Ok(Self::StableMetadata),
            Ok("client-id-copy") => Ok(Self::ClientIdCopy),
            Ok("membership") | Err(env::VarError::NotPresent) => Ok(Self::Membership),
            Ok(value) => Err(format!("unknown FR_BENCH_LEVER {value:?}")),
            Err(error) => Err(format!("invalid FR_BENCH_LEVER: {error}")),
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "membership" => Ok(Self::Membership),
            "snapshot-reuse" => Ok(Self::SnapshotReuse),
            "transaction-reuse" => Ok(Self::TransactionReuse),
            "tracking-reuse" => Ok(Self::TrackingReuse),
            "last-command-reuse" => Ok(Self::LastCommandReuse),
            "transaction-pristine" => Ok(Self::TransactionPristine),
            "stable-metadata" => Ok(Self::StableMetadata),
            "client-id-copy" => Ok(Self::ClientIdCopy),
            _ => Err(format!("unknown lever {value:?}")),
        }
    }
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

    const fn profile_symbol(self, lever: Lever) -> &'static str {
        match (lever, self) {
            (Lever::Membership, Self::Candidate) => {
                "<fr_runtime::Runtime>::record_client_session_insert_reference"
            }
            (Lever::Membership, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session_refresh_reference"
            }
            (Lever::SnapshotReuse, Self::Candidate) => {
                "<fr_runtime::ClientSession as core::clone::Clone>::clone_from"
            }
            (Lever::SnapshotReuse, Self::Reference) => {
                "<fr_runtime::ClientSession as core::clone::Clone>::clone"
            }
            (Lever::TransactionReuse, Self::Candidate) => {
                "<fr_runtime::TransactionState as core::clone::Clone>::clone_from"
            }
            (Lever::TransactionReuse, Self::Reference) => {
                "<fr_runtime::TransactionState>::replace_from_clone_reference"
            }
            (Lever::TrackingReuse, Self::Candidate) => {
                "<fr_runtime::Runtime>::record_client_session"
            }
            (Lever::TrackingReuse, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session_tracking_replace_reference"
            }
            (Lever::LastCommandReuse, Self::Candidate) => {
                "<fr_runtime::Runtime>::record_client_session"
            }
            (Lever::LastCommandReuse, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session_last_command_copy_reference"
            }
            (Lever::TransactionPristine, Self::Candidate) => {
                "<fr_runtime::Runtime>::record_client_session"
            }
            (Lever::TransactionPristine, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session_transaction_pristine_reference"
            }
            (Lever::StableMetadata, Self::Candidate) => {
                "<fr_runtime::Runtime>::record_client_session"
            }
            (Lever::StableMetadata, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session_stable_metadata_reference"
            }
            (Lever::ClientIdCopy, Self::Candidate) => {
                "<fr_runtime::Runtime>::record_client_session"
            }
            (Lever::ClientIdCopy, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session_client_id_copy_reference"
            }
        }
    }

    const fn wrong_profile_symbol(self, lever: Lever) -> &'static str {
        match (lever, self) {
            (Lever::Membership, Self::Candidate) => "record_client_session_refresh_reference",
            (Lever::Membership, Self::Reference) => "record_client_session_insert_reference",
            (Lever::SnapshotReuse, Self::Candidate) => {
                "<fr_runtime::ClientSession as core::clone::Clone>::clone "
            }
            (Lever::SnapshotReuse, Self::Reference) => "clone_from",
            (Lever::TransactionReuse, Self::Candidate) => "replace_from_clone_reference",
            (Lever::TransactionReuse, Self::Reference) => {
                "<fr_runtime::TransactionState as core::clone::Clone>::clone_from"
            }
            (Lever::TrackingReuse, Self::Candidate) => {
                "record_client_session_tracking_replace_reference"
            }
            (Lever::TrackingReuse, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session "
            }
            (Lever::LastCommandReuse, Self::Candidate) => {
                "record_client_session_last_command_copy_reference"
            }
            (Lever::LastCommandReuse, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session "
            }
            (Lever::TransactionPristine, Self::Candidate) => {
                "record_client_session_transaction_pristine_reference"
            }
            (Lever::TransactionPristine, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session "
            }
            (Lever::StableMetadata, Self::Candidate) => {
                "record_client_session_stable_metadata_reference"
            }
            (Lever::StableMetadata, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session "
            }
            (Lever::ClientIdCopy, Self::Candidate) => {
                "record_client_session_client_id_copy_reference"
            }
            (Lever::ClientIdCopy, Self::Reference) => {
                "<fr_runtime::Runtime>::record_client_session "
            }
        }
    }
}

fn record(runtime: &mut Runtime, session: &fr_runtime::ClientSession, lever: Lever, arm: Arm) {
    match (lever, arm) {
        (Lever::Membership, Arm::Candidate) | (Lever::SnapshotReuse, Arm::Reference) => {
            runtime.record_client_session_insert_reference(session);
        }
        (Lever::Membership, Arm::Reference) => {
            runtime.record_client_session_refresh_reference(session);
        }
        (Lever::SnapshotReuse, Arm::Candidate) => runtime.record_client_session(session),
        (Lever::TransactionReuse, Arm::Candidate) => runtime.record_client_session(session),
        (Lever::TransactionReuse, Arm::Reference) => {
            runtime.record_client_session_transaction_replace_reference(session);
        }
        (Lever::TrackingReuse, Arm::Candidate) => runtime.record_client_session(session),
        (Lever::TrackingReuse, Arm::Reference) => {
            runtime.record_client_session_tracking_replace_reference(session);
        }
        (Lever::LastCommandReuse, Arm::Candidate) => runtime.record_client_session(session),
        (Lever::LastCommandReuse, Arm::Reference) => {
            runtime.record_client_session_last_command_copy_reference(session);
        }
        (Lever::TransactionPristine, Arm::Candidate) => runtime.record_client_session(session),
        (Lever::TransactionPristine, Arm::Reference) => {
            runtime.record_client_session_transaction_pristine_reference(session);
        }
        (Lever::StableMetadata, Arm::Candidate) => runtime.record_client_session(session),
        (Lever::StableMetadata, Arm::Reference) => {
            runtime.record_client_session_stable_metadata_reference(session);
        }
        (Lever::ClientIdCopy, Arm::Candidate) => runtime.record_client_session(session),
        (Lever::ClientIdCopy, Arm::Reference) => {
            runtime.record_client_session_client_id_copy_reference(session);
        }
    }
}

fn run_loop(lever: Lever, arm: Arm, repeats: usize) {
    let mut runtime = Runtime::default_strict();
    let session = if matches!(lever, Lever::LastCommandReuse) {
        let session = runtime.new_session();
        let previous = runtime.swap_session(session);
        assert_eq!(
            runtime.execute_frame(command(&[b"PING"]), 1),
            RespFrame::SimpleString("PONG".to_owned())
        );
        runtime.swap_session(previous)
    } else {
        runtime.new_session()
    };
    record(&mut runtime, &session, lever, arm);
    for _ in 0..repeats {
        record(black_box(&mut runtime), black_box(&session), lever, arm);
    }
    black_box(runtime);
}

fn command(parts: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some(part.to_vec())))
            .collect(),
    ))
}

fn bcast_sequence(lever: Lever, arm: Arm) -> (RespFrame, Vec<fr_store::PubSubMessage>, RespFrame) {
    let mut runtime = Runtime::default_strict();
    let tracker = runtime.new_session();
    let writer = runtime.new_session();
    let previous = runtime.swap_session(tracker);
    let tracking_reply = runtime.execute_frame(
        command(&[b"CLIENT", b"TRACKING", b"ON", b"BCAST", b"PREFIX", b"hot:"]),
        1,
    );
    let tracker = runtime.swap_session(writer);
    record(&mut runtime, &tracker, lever, arm);
    let write_reply = runtime.execute_frame(command(&[b"SET", b"hot:key", b"value"]), 2);
    let invalidations = runtime.drain_pubsub_for_client(tracker.client_id);
    let _ = runtime.swap_session(previous);
    (tracking_reply, invalidations, write_reply)
}

fn disabled_sequence(lever: Lever, arm: Arm) -> Vec<fr_store::PubSubMessage> {
    let mut runtime = Runtime::default_strict();
    let tracker = runtime.new_session();
    let writer = runtime.new_session();
    let previous = runtime.swap_session(tracker);
    assert_eq!(
        runtime.execute_frame(command(&[b"CLIENT", b"TRACKING", b"ON", b"BCAST"]), 1),
        RespFrame::SimpleString("OK".to_owned())
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"CLIENT", b"TRACKING", b"OFF"]), 2),
        RespFrame::SimpleString("OK".to_owned())
    );
    let tracker = runtime.swap_session(writer);
    record(&mut runtime, &tracker, lever, arm);
    assert_eq!(
        runtime.execute_frame(command(&[b"SET", b"cold:key", b"value"]), 3),
        RespFrame::SimpleString("OK".to_owned())
    );
    let invalidations = runtime.drain_pubsub_for_client(tracker.client_id);
    let _ = runtime.swap_session(previous);
    invalidations
}

fn transaction_snapshot(lever: Lever, arm: Arm) -> String {
    let mut runtime = Runtime::default_strict();
    let session = runtime.new_session();
    record(&mut runtime, &session, lever, arm);
    let previous = runtime.swap_session(session);
    assert_eq!(
        runtime.execute_frame(command(&[b"WATCH", b"watched:key"]), 1),
        RespFrame::SimpleString("OK".to_owned())
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"MULTI"]), 2),
        RespFrame::SimpleString("OK".to_owned())
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"SET", b"queued:key", b"value"]), 3),
        RespFrame::SimpleString("QUEUED".to_owned())
    );
    let updated = runtime.swap_session(previous);
    record(&mut runtime, &updated, lever, arm);
    runtime
        .recorded_transaction_state_debug(updated.client_id)
        .expect("recorded transaction snapshot")
}

fn tracking_snapshot(lever: Lever, arm: Arm) -> String {
    let mut runtime = Runtime::default_strict();
    let session = runtime.new_session();
    record(&mut runtime, &session, lever, arm);
    let previous = runtime.swap_session(session);
    assert_eq!(
        runtime.execute_frame(
            command(&[b"CLIENT", b"TRACKING", b"ON", b"BCAST", b"PREFIX", b"hot:"]),
            1,
        ),
        RespFrame::SimpleString("OK".to_owned())
    );
    let updated = runtime.swap_session(previous);
    record(&mut runtime, &updated, lever, arm);
    runtime
        .recorded_tracking_state_debug(updated.client_id)
        .expect("recorded tracking snapshot")
}

fn last_command_snapshot(lever: Lever, arm: Arm, parts: &[&[u8]]) -> String {
    let mut runtime = Runtime::default_strict();
    let session = runtime.new_session();
    record(&mut runtime, &session, lever, arm);
    let previous = runtime.swap_session(session);
    let _ = runtime.execute_frame(command(parts), 1);
    let updated = runtime.swap_session(previous);
    record(&mut runtime, &updated, lever, arm);
    record(&mut runtime, &updated, lever, arm);
    runtime
        .recorded_last_command_name_debug(updated.client_id)
        .expect("recorded last command name")
}

fn stable_metadata_snapshot(lever: Lever, arm: Arm) -> String {
    let mut runtime = Runtime::default_strict();
    let mut session = runtime.new_session();
    session.client_id = 42;
    record(&mut runtime, &session, lever, arm);
    let previous = runtime.swap_session(session);
    assert!(matches!(
        runtime.execute_frame(command(&[b"HELLO", b"3"]), 1),
        RespFrame::Map(Some(_))
    ));
    assert_eq!(
        runtime.execute_frame(command(&[b"SELECT", b"7"]), 2),
        RespFrame::SimpleString("OK".to_owned())
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"CLIENT", b"SETNAME", b"metadata"]), 3),
        RespFrame::SimpleString("OK".to_owned())
    );
    assert_eq!(
        runtime.execute_frame(
            command(&[b"CLIENT", b"SETINFO", b"LIB-NAME", b"fr-bench"]),
            4,
        ),
        RespFrame::SimpleString("OK".to_owned())
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"CLIENT", b"SETINFO", b"LIB-VER", b"7.2.4"]), 5,),
        RespFrame::SimpleString("OK".to_owned())
    );
    let updated = runtime.swap_session(previous);
    record(&mut runtime, &updated, lever, arm);
    record(&mut runtime, &updated, lever, arm);
    runtime
        .recorded_client_session_debug(updated.client_id)
        .expect("recorded client session")
}

fn correctness_gate(lever: Lever) {
    let candidate = bcast_sequence(lever, Arm::Candidate);
    let reference = bcast_sequence(lever, Arm::Reference);
    assert_eq!(candidate, reference);
    assert_eq!(
        candidate.1,
        vec![fr_store::PubSubMessage::Invalidate {
            keys: vec![b"hot:key".to_vec()]
        }]
    );
    assert_eq!(disabled_sequence(lever, Arm::Candidate), Vec::new());
    assert_eq!(disabled_sequence(lever, Arm::Reference), Vec::new());
    if matches!(lever, Lever::TransactionReuse | Lever::TransactionPristine) {
        assert_eq!(
            transaction_snapshot(lever, Arm::Candidate),
            transaction_snapshot(lever, Arm::Reference)
        );
    }
    if matches!(lever, Lever::TrackingReuse) {
        assert_eq!(
            tracking_snapshot(lever, Arm::Candidate),
            tracking_snapshot(lever, Arm::Reference)
        );
    }
    if matches!(lever, Lever::LastCommandReuse) {
        assert_eq!(
            last_command_snapshot(lever, Arm::Candidate, &[b"PING"]),
            last_command_snapshot(lever, Arm::Reference, &[b"PING"])
        );
        assert_eq!(
            last_command_snapshot(lever, Arm::Candidate, &[b"ECHO", b"value"]),
            last_command_snapshot(lever, Arm::Reference, &[b"ECHO", b"value"])
        );
    }
    if matches!(lever, Lever::StableMetadata | Lever::ClientIdCopy) {
        assert_eq!(
            stable_metadata_snapshot(lever, Arm::Candidate),
            stable_metadata_snapshot(lever, Arm::Reference)
        );
    }
    println!(
        "CORRECTNESS_GATE identical=true cases=bcast_enabled,bcast_disabled,transaction_snapshot mutation_sites=client_tracking"
    );
}

fn child_args() -> Result<Option<(Lever, Arm, usize)>, String> {
    let args = env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) != Some("--child") {
        return Ok(None);
    }
    let lever = Lever::parse(args.get(2).ok_or("missing child lever")?)?;
    let arm = Arm::parse(args.get(3).ok_or("missing child arm")?)?;
    let repeats = args
        .get(4)
        .ok_or("missing child repeat count")?
        .parse::<usize>()
        .map_err(|error| format!("invalid repeat count: {error}"))?;
    Ok(Some((lever, arm, repeats)))
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

fn profile_trial(executable: &Path, lever: Lever, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_runtime_tracking_membership_{}_{}_{}_{}.data",
        process::id(),
        lever.name(),
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
            "60s",
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
        .args([
            "--child",
            lever.name(),
            arm.name(),
            &PROFILE_REPEATS.to_string(),
        ])
        .output()
        .map_err(|error| format!("perf record launch failed: {error}"))?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let report = Command::new("timeout")
        .env("LC_ALL", "C")
        .args(["--foreground", "30s", "perf", "report", "-i"])
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
        .map_err(|error| format!("perf report launch failed: {error}"))?;
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
        .ok_or_else(|| "perf report omitted Total Lost Samples".to_owned())?
        .rsplit(':')
        .next()
        .ok_or_else(|| "missing lost-sample count".to_owned())?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost != 0 {
        return Err(format!("profile lost {lost} samples"));
    }
    if stdout
        .lines()
        .any(|line| line.contains(arm.wrong_profile_symbol(lever)))
    {
        return Err(format!("{} profile executed wrong helper", arm.name()));
    }
    let line = stdout
        .lines()
        .find(|line| line.contains(arm.profile_symbol(lever)))
        .ok_or_else(|| format!("profile has no exact {} helper frame", arm.name()))?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or_else(|| "missing self-time".to_owned())?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err(format!("{} helper has zero self-time", arm.name()));
    }
    Ok(self_pct)
}

fn perf_instructions(executable: &Path, lever: Lever, arm: Arm) -> Result<u64, String> {
    let output = Command::new("timeout")
        .env("LC_ALL", "C")
        .args([
            "--foreground",
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
        .args([
            "--child",
            lever.name(),
            arm.name(),
            &STAT_REPEATS.to_string(),
        ])
        .output()
        .map_err(|error| format!("perf stat launch failed: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat failed: {stderr}"));
    }
    stderr
        .lines()
        .find_map(|line| {
            let fields = line.split(',').collect::<Vec<_>>();
            fields
                .iter()
                .any(|field| field.trim().contains("instructions"))
                .then(|| fields[0].trim())
        })
        .ok_or_else(|| format!("instructions:u missing: {stderr}"))?
        .parse::<u64>()
        .map_err(|error| format!("invalid instruction count: {error}"))
}

fn run_instruction_ab(executable: &Path, lever: Lever) -> Result<(), String> {
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
            counts[slot] = perf_instructions(executable, lever, arm)?;
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
    if null_cv_pct >= 5.0 || effect_cv_pct >= 5.0 {
        return Err(format!(
            "CV gate failed: null={null_cv_pct:.6}% effect={effect_cv_pct:.6}%"
        ));
    }
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
    if let Some((lever, arm, repeats)) = child_args()? {
        run_loop(lever, arm, repeats);
        return Ok(());
    }
    let lever = Lever::from_env()?;
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve benchmark executable: {error}"))?;
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(&executable)?);
    println!(
        "TRIGGER lever={} operation=record_client_session existing_snapshot=true tracking_enabled=false tracking_bcast=false bcast_index_empty=true",
        lever.name()
    );
    correctness_gate(lever);
    for arm in [Arm::Candidate, Arm::Reference] {
        let warm = Command::new(&executable)
            .args(["--child", lever.name(), arm.name(), "1000"])
            .status()
            .map_err(|error| format!("warm-up launch failed: {error}"))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
        let self_pct = profile_trial(&executable, lever, arm)?;
        println!("PROFILE_SELF arm={} self_pct={self_pct:.4}", arm.name());
    }
    run_instruction_ab(&executable, lever)
}
