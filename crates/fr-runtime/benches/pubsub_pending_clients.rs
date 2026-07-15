//! Same-binary proof for draining pending pub/sub client outboxes.
//!
//! The current server snapshots every pending client ID and then hashes every ID again to remove
//! its outbox. This harness includes the real `PUBLISH` refill before comparing that delivery loop
//! with a one-pass, capacity-retaining map drain.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_runtime::Runtime;

const CLIENTS: usize = 256;
const PROFILE_REPEATS: usize = 20_000;
const STAT_REPEATS: usize = 2_000;
const STAT_ROUNDS: usize = 11;

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
            Self::Candidate => "drain_candidate",
            Self::Reference => "drain_reference",
        }
    }
}

fn runtime_with_subscribers(client_count: usize) -> Runtime {
    let mut runtime = Runtime::default_strict();
    for _ in 0..client_count {
        let subscriber = runtime.new_session();
        let publisher = runtime.swap_session(subscriber);
        runtime.pubsub_subscribe(b"events".to_vec());
        runtime.swap_session(publisher);
    }
    runtime
}

#[derive(Debug, PartialEq, Eq)]
struct DrainSummary {
    clients: usize,
    messages: usize,
    client_id_checksum: u64,
}

impl DrainSummary {
    const fn new() -> Self {
        Self {
            clients: 0,
            messages: 0,
            client_id_checksum: 0,
        }
    }

    fn record(&mut self, client_id: u64, messages: Vec<fr_store::PubSubMessage>) {
        self.clients += 1;
        self.messages += messages.len();
        self.client_id_checksum = self.client_id_checksum.wrapping_add(client_id);
        black_box(messages);
    }
}

#[inline(never)]
fn drain_candidate(runtime: &mut Runtime) -> DrainSummary {
    let mut summary = DrainSummary::new();
    for (client_id, messages) in black_box(runtime).drain_pubsub_outboxes() {
        summary.record(client_id, messages);
    }
    black_box(summary)
}

/// Frozen pre-optimization server loop: snapshot IDs, then hash each ID again to remove its
/// messages. Keeping the consumer in this wrapper gives both arms exactly one result sink without
/// adding a second output vector to the reference arm.
#[inline(never)]
fn drain_reference(runtime: &mut Runtime) -> DrainSummary {
    let client_ids = black_box(runtime.pubsub_clients_with_pending());
    let mut summary = DrainSummary::new();
    for client_id in client_ids {
        let messages = black_box(runtime.drain_pubsub_for_client(client_id));
        summary.record(client_id, messages);
    }
    black_box(summary)
}

fn drain_summary(runtime: &mut Runtime, arm: Arm) -> DrainSummary {
    match arm {
        Arm::Candidate => drain_candidate(runtime),
        Arm::Reference => drain_reference(runtime),
    }
}

fn collect_candidate(runtime: &mut Runtime) -> Vec<(u64, Vec<fr_store::PubSubMessage>)> {
    runtime.drain_pubsub_outboxes()
}

fn collect_reference(runtime: &mut Runtime) -> Vec<(u64, Vec<fr_store::PubSubMessage>)> {
    runtime
        .pubsub_clients_with_pending()
        .into_iter()
        .map(|client_id| (client_id, runtime.drain_pubsub_for_client(client_id)))
        .collect()
}

fn run_loop(arm: Arm, repeats: usize, client_count: usize) {
    let mut runtime = runtime_with_subscribers(client_count);
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let receivers = runtime.pubsub_publish(black_box(b"events"), black_box(b"payload"));
        let summary = drain_summary(black_box(&mut runtime), arm);
        checksum = checksum.wrapping_add(receivers as u64);
        checksum = checksum.wrapping_add(summary.clients as u64);
        checksum = checksum.wrapping_add(summary.messages as u64);
        checksum = checksum.wrapping_add(summary.client_id_checksum);
    }
    black_box(checksum);
}

fn correctness_gate() {
    for client_count in [0, 1, 8, CLIENTS] {
        let mut runtime = runtime_with_subscribers(client_count);
        assert_eq!(runtime.pubsub_publish(b"events", b"payload"), client_count);
        let mut candidate = collect_candidate(&mut runtime);
        assert_eq!(runtime.pubsub_publish(b"events", b"payload"), client_count);
        let mut reference = collect_reference(&mut runtime);
        candidate.sort_unstable_by_key(|(client_id, _)| *client_id);
        reference.sort_unstable_by_key(|(client_id, _)| *client_id);
        assert_eq!(candidate, reference);
        assert_eq!(candidate.len(), client_count);
        assert!(runtime.pubsub_clients_with_pending().is_empty());
    }
    println!("CORRECTNESS_GATE outboxes=identical sizes=0,1,8,{CLIENTS}");
}

fn child_args() -> Result<Option<(Arm, usize, usize)>, String> {
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
    let client_count = args
        .get(4)
        .ok_or("missing child client count")?
        .parse()
        .map_err(|error| format!("invalid client count: {error}"))?;
    Ok(Some((arm, repeats, client_count)))
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

fn profile_trial(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_pubsub_pending_{}_{}_{}.data",
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
        .args([
            "--child",
            arm.name(),
            &PROFILE_REPEATS.to_string(),
            &CLIENTS.to_string(),
        ])
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
        "PROFILE_TABLE_BEGIN arm={}\n{stdout}\nPROFILE_TABLE_END arm={}",
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

fn worker_id() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn run_profile(executable: &Path, arms: &[Arm]) -> Result<(), String> {
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!("TRIGGER subscribers={CLIENTS} publish_then_drain=true");
    for &arm in arms {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "1000", &CLIENTS.to_string()])
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
        .args([
            "--child",
            arm.name(),
            &STAT_REPEATS.to_string(),
            &CLIENTS.to_string(),
        ])
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
    if let Some((arm, repeats, client_count)) = child_args()? {
        run_loop(arm, repeats, client_count);
        return Ok(());
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    correctness_gate();
    let reference_profile_only = env::args().any(|arg| arg == "--profile-reference-only");
    if reference_profile_only {
        run_profile(&executable, &[Arm::Reference])
            .map_err(|error| format!("PROFILE INVALID: {error}"))?;
        return Ok(());
    }
    run_profile(&executable, &[Arm::Candidate, Arm::Reference])
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
