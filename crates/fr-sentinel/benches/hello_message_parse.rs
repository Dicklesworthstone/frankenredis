//! Same-binary proof for fixed-width Sentinel HELLO parsing.
//!
//! The frozen reference collects every comma-delimited field into a temporary `Vec`. The
//! candidate extracts the exact eight fields directly from the split iterator.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_sentinel::{
    SentinelState,
    discovery::{
        DiscoveryAction, HelloMessage, bench_parse_hello_collect_reference,
        bench_process_hello_owned_self_id_reference, process_hello_message,
    },
};

const MESSAGE: &str =
    "192.0.2.10,26379,0123456789abcdef0123456789abcdef01234567,42,mymaster,198.51.100.20,6379,17";
const PROFILE_REPEATS: usize = 250_000;
const PROCESS_SELF_PROFILE_REPEATS: usize = 5_000_000;
const STAT_REPEATS: usize = 100_000;
const STAT_ROUNDS: usize = 9;

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
            Self::Candidate => "<fr_sentinel::discovery::HelloMessage>::parse",
            Self::Reference => "bench_parse_hello_collect_reference",
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "bench_parse_hello_collect_reference",
            Self::Reference => "<fr_sentinel::discovery::HelloMessage>::parse",
        }
    }
}

#[derive(Clone, Copy)]
enum ProcessArm {
    Candidate,
    Reference,
}

impl ProcessArm {
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
            _ => Err(format!("unknown process arm {value:?}")),
        }
    }

    const fn profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "fr_sentinel::discovery::process_hello_message",
            Self::Reference => "bench_process_hello_owned_self_id_reference",
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "bench_process_hello_owned_self_id_reference",
            Self::Reference => "fr_sentinel::discovery::process_hello_message",
        }
    }
}

fn parse(message: &str, arm: Arm) -> Option<HelloMessage> {
    match arm {
        Arm::Candidate => HelloMessage::parse(black_box(message)),
        Arm::Reference => bench_parse_hello_collect_reference(black_box(message)),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let parsed = parse(black_box(MESSAGE), arm);
        if let Some(message) = black_box(&parsed) {
            checksum = checksum
                .wrapping_add(u64::from(message.sentinel_port))
                .wrapping_add(message.current_epoch)
                .wrapping_add(message.master_config_epoch)
                .wrapping_add(message.sentinel_ip.len() as u64)
                .wrapping_add(message.sentinel_runid.len() as u64)
                .wrapping_add(message.master_name.len() as u64)
                .wrapping_add(message.master_ip.len() as u64);
        }
        black_box(parsed);
    }
    black_box(checksum);
}

fn self_hello_fixture() -> Result<(SentinelState, HelloMessage), String> {
    let mut state = SentinelState::new();
    state
        .monitor("mymaster", "198.51.100.20", 6379, 2)
        .map_err(str::to_owned)?;
    let hello = HelloMessage {
        sentinel_ip: "192.0.2.10".to_owned(),
        sentinel_port: 26379,
        sentinel_runid: state.myid_hex(),
        current_epoch: 42,
        master_name: "mymaster".to_owned(),
        master_ip: "198.51.100.20".to_owned(),
        master_port: 6379,
        master_config_epoch: 17,
    };
    Ok((state, hello))
}

fn process_hello(state: &SentinelState, hello: &HelloMessage, arm: ProcessArm) -> DiscoveryAction {
    match arm {
        ProcessArm::Candidate => process_hello_message(state, hello, 1_000),
        ProcessArm::Reference => bench_process_hello_owned_self_id_reference(state, hello, 1_000),
    }
}

fn run_process_self_loop(arm: ProcessArm, repeats: usize) -> Result<(), String> {
    let (state, hello) = self_hello_fixture()?;
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let action = process_hello(black_box(&state), black_box(&hello), arm);
        checksum = checksum.wrapping_add(match black_box(&action) {
            DiscoveryAction::None => 1,
            DiscoveryAction::AddSentinel { .. } => 2,
            DiscoveryAction::UpdateSentinel { .. } => 3,
        });
        black_box(action);
    }
    black_box(checksum);
    Ok(())
}

fn process_correctness_gate() -> Result<(), String> {
    let (state, hello) = self_hello_fixture()?;
    let self_candidate = process_hello(&state, &hello, ProcessArm::Candidate);
    let self_reference = process_hello(&state, &hello, ProcessArm::Reference);
    assert_eq!(self_candidate, self_reference);
    assert_eq!(self_candidate, DiscoveryAction::None);

    let mut other = hello.clone();
    other.sentinel_runid = "other-sentinel-runid".to_owned();
    let other_candidate = process_hello(&state, &other, ProcessArm::Candidate);
    let other_reference = process_hello(&state, &other, ProcessArm::Reference);
    assert_eq!(other_candidate, other_reference);
    assert!(matches!(
        other_candidate,
        DiscoveryAction::AddSentinel { .. }
    ));

    let mut missing = other.clone();
    missing.master_name = "missing-master".to_owned();
    assert_eq!(
        process_hello(&state, &missing, ProcessArm::Candidate),
        process_hello(&state, &missing, ProcessArm::Reference)
    );

    let mut invalid_state = state;
    invalid_state.myid = [0xff; 40];
    let mut invalid_self = hello;
    invalid_self.sentinel_runid = String::from_utf8_lossy(&invalid_state.myid).into_owned();
    let invalid_candidate = process_hello(&invalid_state, &invalid_self, ProcessArm::Candidate);
    let invalid_reference = process_hello(&invalid_state, &invalid_self, ProcessArm::Reference);
    assert_eq!(invalid_candidate, invalid_reference);
    assert_eq!(invalid_candidate, DiscoveryAction::None);

    println!(
        "CORRECTNESS_GATE process_self=identical cases=4 valid_self_nonself_missing_master_invalid_utf8_myid=covered"
    );
    Ok(())
}

fn correctness_gate() {
    let cases = [
        MESSAGE,
        "192.0.2.1,abc,runid,12epoch,mymaster,10.0.0.1,6379tail,7cfg",
        "ip, 26379tail,, +12epoch,,host, 6379tail,-7cfg",
        "ip,-1,runid,1,mymaster,host,6379,1",
        "ip,65536,runid,1,mymaster,host,6379,1",
        "",
        "one",
        "one,two,three,four,five,six,seven",
        "one,two,three,four,five,six,seven,eight,nine",
        "one,two,three,four,five,six,seven,eight,",
    ];
    for (index, message) in cases.iter().enumerate() {
        assert_eq!(
            parse(message, Arm::Candidate),
            parse(message, Arm::Reference),
            "parser differs for case {index}: {message:?}"
        );
    }
    println!(
        "CORRECTNESS_GATE parser=identical cases={} valid_invalid_extra_empty_numeric_prefixes=covered",
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

fn process_self_child_args() -> Result<Option<(ProcessArm, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--process-self-child") {
        return Ok(None);
    }
    let arm = ProcessArm::parse(args.get(2).ok_or("missing process-self arm")?)?;
    let repeats = args
        .get(3)
        .ok_or("missing process-self repeat count")?
        .parse()
        .map_err(|error| format!("invalid process-self repeat count: {error}"))?;
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
        "fr_sentinel_hello_parse_{}_{}_{}.data",
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
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!(
        "TRIGGER bytes={} fields=8 valid_sentinel_hello=true",
        MESSAGE.len()
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

fn profile_process_self_trial(executable: &Path, arm: ProcessArm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_sentinel_process_self_hello_{}_{}_{}.data",
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
            "--process-self-child",
            arm.name(),
            &PROCESS_SELF_PROFILE_REPEATS.to_string(),
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
        "PROFILE_TABLE_BEGIN scenario=process-self arm={}\n{stdout}\nPROFILE_TABLE_END scenario=process-self arm={}",
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
            "{} process profile executed wrong helper {}",
            arm.name(),
            arm.wrong_profile_symbol()
        ));
    }
    let line = stdout
        .lines()
        .find(|line| line.contains(arm.profile_symbol()))
        .ok_or_else(|| {
            format!(
                "profile has no exact {} process helper frame; workload INVALID",
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
        return Err(format!(
            "{} process helper has zero self-time; workload INVALID",
            arm.name()
        ));
    }
    Ok(self_pct)
}

fn run_process_self_profile(executable: &Path, arms: &[ProcessArm]) -> Result<(), String> {
    process_correctness_gate()?;
    let (state, hello) = self_hello_fixture()?;
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!(
        "TRIGGER scenario=process-self runid_bytes={} monitored_masters={} action=none",
        hello.sentinel_runid.len(),
        state.masters.len()
    );
    for &arm in arms {
        let status = Command::new(executable)
            .args(["--process-self-child", arm.name(), "100"])
            .status()
            .map_err(|error| format!("could not launch process-self warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} process-self warm-up failed", arm.name()));
        }
        let self_pct = profile_process_self_trial(executable, arm)?;
        println!(
            "PROFILE_SELF scenario=process-self arm={} self_pct={self_pct:.4}",
            arm.name()
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

fn perf_process_instructions(executable: &Path, arm: ProcessArm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args([
            "--process-self-child",
            arm.name(),
            &STAT_REPEATS.to_string(),
        ])
        .output()
        .map_err(|error| format!("could not launch process perf stat: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("process perf stat failed: {stderr}"));
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
        .ok_or_else(|| format!("process instructions:u missing: {stderr}"))?
        .parse()
        .map_err(|error| format!("invalid process instruction count: {error}"))
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

fn run_process_self_instruction_ab(executable: &Path) -> Result<(), String> {
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
                ProcessArm::Reference
            } else {
                ProcessArm::Candidate
            };
            counts[slot] = perf_process_instructions(executable, arm)?;
        }
        let null = counts[0] as f64 / counts[1] as f64;
        let effect = counts[2] as f64 / counts[0] as f64;
        println!(
            "PROCESS_INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} null_ratio={null:.9} reference_over_candidate={effect:.9}",
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
        "PROCESS_INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} speedup_cv_pct={effect_cv_pct:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!(
            "process null median exposes harness bias: {null_median:.9}"
        ));
    }
    if effect_median <= null_p95 || effect_median <= 1.01 {
        return Err(format!(
            "process candidate failed keep gate: effect={effect_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    Ok(())
}

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = process_self_child_args()? {
        return run_process_self_loop(arm, repeats);
    }
    if let Some((arm, repeats)) = child_args()? {
        run_loop(arm, repeats);
        return Ok(());
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    if env::args().any(|arg| arg == "--profile-process-self-only") {
        return run_process_self_profile(&executable, &[ProcessArm::Candidate])
            .map_err(|error| format!("PROFILE INVALID: {error}"));
    }
    if env::args().any(|arg| arg == "--process-self-ab") {
        run_process_self_profile(&executable, &[ProcessArm::Candidate, ProcessArm::Reference])
            .map_err(|error| format!("PROFILE INVALID: {error}"))?;
        return run_process_self_instruction_ab(&executable)
            .map_err(|error| format!("A/B INVALID: {error}"));
    }
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
