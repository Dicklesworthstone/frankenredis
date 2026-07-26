//! Corrected-harness reruns for Meta-Lever 1 resurrection queue rows 3-5.
//!
//! One outer invocation:
//! - hashes its own executable and prints that identity as line one;
//! - proves the reference and candidate results byte-for-byte equivalent;
//! - profile-verifies non-zero exact reference self-time;
//! - measures reference/reference A/A and reference/candidate A/B in the same
//!   position-balanced `perf stat` routine; and
//! - gates only on the bootstrap 95% CI of the A/A median, never on CV.

use std::{
    env, fs,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_store::{MaxmemoryPolicy, SmembersScanEvent, SscanReplyEvent, Store, StreamField};
use sha2::{Digest, Sha256};

const STAT_ROUNDS: usize = 24;
const PROFILE_TRIALS: usize = 3;
const BOOTSTRAP_RESAMPLES: usize = 20_000;

#[derive(Clone, Copy, Debug)]
enum Scenario {
    ScanClone,
    SetExpiry,
    XaddExpiry,
    ZrangebylexLfu,
}

impl Scenario {
    const ALL: [Self; 4] = [
        Self::ScanClone,
        Self::SetExpiry,
        Self::XaddExpiry,
        Self::ZrangebylexLfu,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::ScanClone => "scan_clone",
            Self::SetExpiry => "set_expiry",
            Self::XaddExpiry => "xadd_expiry",
            Self::ZrangebylexLfu => "zrangebylex_lfu",
        }
    }

    const fn reference_symbol(self) -> &'static str {
        match self {
            Self::ScanClone => "::sscan",
            Self::SetExpiry => "set_plain_borrowed_expiry_reference_bench",
            Self::XaddExpiry => "xadd_expiry_reference_bench",
            Self::ZrangebylexLfu => "zrangebylex_members_borrow_scan_lfu_twoprobe_bench",
        }
    }

    const fn stat_repeats(self) -> usize {
        match self {
            Self::ScanClone => 200_000,
            Self::SetExpiry => 2_000_000,
            Self::XaddExpiry => 300_000,
            Self::ZrangebylexLfu => 200_000,
        }
    }

    const fn profile_repeats(self) -> usize {
        match self {
            Self::ScanClone => 2_000_000,
            Self::SetExpiry => 20_000_000,
            Self::XaddExpiry => 2_000_000,
            Self::ZrangebylexLfu => 2_000_000,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|scenario| scenario.name() == value)
            .ok_or_else(|| format!("unknown scenario {value:?}"))
    }
}

#[derive(Clone, Copy, Debug)]
enum Arm {
    Reference,
    Candidate,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Candidate => "candidate",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "reference" => Ok(Self::Reference),
            "candidate" => Ok(Self::Candidate),
            _ => Err(format!("unknown arm {value:?}")),
        }
    }
}

fn binary_identity(executable: &Path) -> Result<(String, usize), String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = fs::read(executable)
        .map_err(|error| format!("could not read {}: {error}", executable.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let mut digest = String::with_capacity(64);
    for byte in hasher.finalize() {
        digest.push(char::from(HEX[usize::from(byte >> 4)]));
        digest.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok((digest, bytes.len()))
}

fn fold_bytes(mut checksum: u64, bytes: &[u8]) -> u64 {
    checksum ^= bytes.len() as u64;
    for byte in bytes {
        checksum = checksum.rotate_left(7) ^ u64::from(*byte);
    }
    checksum
}

fn fold_word(checksum: u64, value: u64) -> u64 {
    checksum.rotate_left(13).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ value
}

fn build_scan_store() -> Store {
    let mut store = Store::new();
    let members: Vec<Vec<u8>> = (0..256)
        .map(|index| {
            let mut member = vec![b'm'; 80];
            member[..8].copy_from_slice(&(index as u64).to_be_bytes());
            member
        })
        .collect();
    store
        .sadd(b"s", &members, 1_000)
        .expect("scan fixture SADD succeeds");
    store
}

fn scan_once(store: &mut Store, arm: Arm) -> u64 {
    match arm {
        Arm::Reference => {
            let (cursor, members) = store
                .sscan(black_box(b"s"), 0, None, 32, 2_000)
                .expect("reference SSCAN succeeds");
            let mut checksum = fold_word(0xcbf2_9ce4_8422_2325, cursor);
            checksum = fold_word(checksum, members.len() as u64);
            for member in members {
                checksum = fold_bytes(checksum, &member);
            }
            checksum
        }
        Arm::Candidate => {
            let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
            store
                .sscan0_borrow_scan(black_box(b"s"), 0, None, 32, 2_000, |event| match event {
                    SscanReplyEvent::Cursor(cursor) => {
                        checksum = fold_word(checksum, cursor);
                    }
                    SscanReplyEvent::Len(len) => {
                        checksum = fold_word(checksum, len as u64);
                    }
                    SscanReplyEvent::Member(member) => {
                        checksum = fold_bytes(checksum, member);
                    }
                })
                .expect("candidate SSCAN succeeds");
            checksum
        }
    }
}

fn run_scan(arm: Arm, repeats: usize) -> u64 {
    let mut store = build_scan_store();
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        checksum = checksum.wrapping_add(scan_once(&mut store, arm));
    }
    black_box(checksum)
}

fn build_set_store() -> Store {
    let mut store = Store::new();
    store.set_plain_borrowed(b"k", b"v0", 1_000);
    store
}

fn set_once(store: &mut Store, arm: Arm) {
    match arm {
        Arm::Reference => {
            store.set_plain_borrowed_expiry_reference_bench(
                black_box(b"k"),
                black_box(b"value"),
                2_000,
            );
        }
        Arm::Candidate => {
            store.set_plain_borrowed(black_box(b"k"), black_box(b"value"), 2_000);
        }
    }
}

fn run_set(arm: Arm, repeats: usize) -> u64 {
    let mut store = build_set_store();
    for _ in 0..repeats {
        set_once(&mut store, arm);
    }
    let value = store
        .get(black_box(b"k"), 2_000)
        .expect("SET result remains string")
        .expect("SET result remains present");
    black_box(fold_bytes(repeats as u64, &value))
}

fn stream_fields() -> Vec<StreamField> {
    vec![(b"field".to_vec(), b"value".to_vec())]
}

fn build_xadd_store(fields: &[StreamField]) -> Store {
    let mut store = Store::new();
    store
        .xadd(b"stream", (1, 0), fields, 1_000)
        .expect("stream fixture XADD succeeds");
    store
}

fn xadd_once(store: &mut Store, arm: Arm, fields: &[StreamField]) {
    let result = match arm {
        Arm::Reference => store.xadd_expiry_reference_bench(
            black_box(b"stream"),
            (1, 0),
            black_box(fields),
            2_000,
        ),
        Arm::Candidate => store.xadd(black_box(b"stream"), (1, 0), black_box(fields), 2_000),
    };
    result.expect("timed XADD succeeds");
}

fn run_xadd(arm: Arm, repeats: usize) -> u64 {
    let fields = stream_fields();
    let mut store = build_xadd_store(&fields);
    for _ in 0..repeats {
        xadd_once(&mut store, arm, &fields);
    }
    let records = store
        .xrange(b"stream", (0, 0), (u64::MAX, u64::MAX), None, 2_000)
        .expect("XADD result remains a stream");
    let mut checksum = records.len() as u64;
    for (id, pairs) in records {
        checksum ^= id.0.rotate_left(11) ^ id.1;
        for (field, value) in pairs {
            checksum = fold_bytes(checksum, &field);
            checksum = fold_bytes(checksum, &value);
        }
    }
    black_box(checksum)
}

fn build_lex_store() -> (Store, Vec<Vec<u8>>) {
    const KEYSPACE: usize = 4_096;
    let mut store = Store::new();
    store.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    store.lfu_decay_time = 0;
    let mut keys = Vec::with_capacity(KEYSPACE);
    for index in 0..KEYSPACE {
        let key = format!("lex:{index:08}").into_bytes();
        store
            .zadd(
                &key,
                &[
                    (1.0, b"member-a".to_vec()),
                    (1.0, b"member-b".to_vec()),
                    (1.0, b"member-c".to_vec()),
                ],
                1_000,
            )
            .expect("lex fixture ZADD succeeds");
        keys.push(key);
    }
    (store, keys)
}

fn lex_once(store: &mut Store, key: &[u8], arm: Arm) -> u64 {
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut sink = |event: SmembersScanEvent<'_>| match event {
        SmembersScanEvent::Len(len) => {
            checksum = fold_word(checksum, len as u64);
        }
        SmembersScanEvent::Member(member) => {
            checksum = fold_bytes(checksum, member);
        }
    };
    match arm {
        Arm::Reference => store
            .zrangebylex_members_borrow_scan_lfu_twoprobe_bench(
                black_box(key),
                black_box(b"-"),
                black_box(b"+"),
                false,
                2_000,
                &mut sink,
            )
            .expect("reference ZRANGEBYLEX succeeds"),
        Arm::Candidate => store
            .zrangebylex_members_borrow_scan(
                black_box(key),
                black_box(b"-"),
                black_box(b"+"),
                false,
                2_000,
                &mut sink,
            )
            .expect("candidate ZRANGEBYLEX succeeds"),
    }
    checksum
}

fn run_lex(arm: Arm, repeats: usize) -> u64 {
    let (mut store, keys) = build_lex_store();
    let mut checksum = 0_u64;
    for index in 0..repeats {
        checksum =
            checksum.wrapping_add(lex_once(&mut store, &keys[index & (keys.len() - 1)], arm));
    }
    let digest = store.state_digest();
    black_box(fold_bytes(checksum, digest.as_bytes()))
}

fn run_loop(scenario: Scenario, arm: Arm, repeats: usize) -> u64 {
    match scenario {
        Scenario::ScanClone => run_scan(arm, repeats),
        Scenario::SetExpiry => run_set(arm, repeats),
        Scenario::XaddExpiry => run_xadd(arm, repeats),
        Scenario::ZrangebylexLfu => run_lex(arm, repeats),
    }
}

fn child_args() -> Result<Option<(Scenario, Arm, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--child") {
        return Ok(None);
    }
    let scenario = Scenario::parse(args.get(2).ok_or("missing child scenario")?)?;
    let arm = Arm::parse(args.get(3).ok_or("missing child arm")?)?;
    let repeats = args
        .get(4)
        .ok_or("missing child repeat count")?
        .parse()
        .map_err(|error| format!("invalid child repeat count: {error}"))?;
    Ok(Some((scenario, arm, repeats)))
}

fn selected_scenarios() -> Result<Vec<Scenario>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--scenario") {
        return Ok(vec![Scenario::parse(
            args.get(2).ok_or("missing selected scenario")?,
        )?]);
    }
    Ok(Scenario::ALL.to_vec())
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
    samples.sort_by(f64::total_cmp);
    let middle = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2.0
    } else {
        samples[middle]
    }
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

fn bootstrap_median_ci(samples: &[f64]) -> (f64, f64) {
    assert!(!samples.is_empty());
    let mut state = 0x4d59_5df4_d0f3_3173_u64;
    for sample in samples {
        state ^= sample.to_bits().wrapping_mul(0x9e37_79b9_7f4a_7c15);
        state = state.rotate_left(17);
    }
    let mut scratch = vec![0.0; samples.len()];
    let mut medians = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        for value in &mut scratch {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let draw = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            *value = samples[(draw as usize) % samples.len()];
        }
        medians.push(median(&mut scratch));
    }
    medians.sort_by(f64::total_cmp);
    (percentile(&medians, 0.025), percentile(&medians, 0.975))
}

fn correctness_gate(scenario: Scenario) {
    match scenario {
        Scenario::ScanClone => {
            let reference = run_scan(Arm::Reference, 1);
            let candidate = run_scan(Arm::Candidate, 1);
            assert_eq!(candidate, reference, "SCAN wire-source sequence differs");
            println!(
                "CORRECTNESS scenario={} result=byte_identical checksum={reference:016x}",
                scenario.name()
            );
        }
        Scenario::SetExpiry => {
            let mut reference = build_set_store();
            let mut candidate = build_set_store();
            set_once(&mut reference, Arm::Reference);
            set_once(&mut candidate, Arm::Candidate);
            let reference_digest = reference.state_digest();
            let candidate_digest = candidate.state_digest();
            assert_eq!(candidate_digest, reference_digest, "SET state differs");
            println!(
                "CORRECTNESS scenario={} result=state_identical checksum={reference_digest}",
                scenario.name()
            );
        }
        Scenario::XaddExpiry => {
            let fields = stream_fields();
            let mut reference = build_xadd_store(&fields);
            let mut candidate = build_xadd_store(&fields);
            xadd_once(&mut reference, Arm::Reference, &fields);
            xadd_once(&mut candidate, Arm::Candidate, &fields);
            let reference_digest = reference.state_digest();
            let candidate_digest = candidate.state_digest();
            assert_eq!(candidate_digest, reference_digest, "XADD state differs");
            println!(
                "CORRECTNESS scenario={} result=state_identical checksum={reference_digest}",
                scenario.name()
            );
        }
        Scenario::ZrangebylexLfu => {
            let (mut reference, keys) = build_lex_store();
            let (mut candidate, candidate_keys) = build_lex_store();
            let reference_result = lex_once(&mut reference, &keys[0], Arm::Reference);
            let candidate_result = lex_once(&mut candidate, &candidate_keys[0], Arm::Candidate);
            assert_eq!(
                candidate_result, reference_result,
                "ZRANGEBYLEX output differs"
            );
            assert_eq!(
                candidate.state_digest(),
                reference.state_digest(),
                "ZRANGEBYLEX LFU state differs"
            );
            println!(
                "CORRECTNESS scenario={} result=state_and_bytes_identical checksum={reference_result:016x}",
                scenario.name()
            );
        }
    }
}

fn profile_trial(executable: &Path, scenario: Scenario, trial: usize) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_resurrection_{}_{}_{}_{}.data",
        scenario.name(),
        process::id(),
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
        .args([
            "--child",
            scenario.name(),
            Arm::Reference.name(),
            &scenario.profile_repeats().to_string(),
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
            "0.1",
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
        "PROFILE_TABLE_BEGIN scenario={} trial={trial}\n{stdout}\nPROFILE_TABLE_END scenario={} trial={trial}",
        scenario.name(),
        scenario.name()
    );
    let self_pct = stdout
        .lines()
        .filter(|line| line.contains(scenario.reference_symbol()))
        .find_map(|line| {
            line.split_whitespace()
                .next()?
                .strip_suffix('%')?
                .parse::<f64>()
                .ok()
        })
        .ok_or_else(|| {
            format!(
                "profile has no numeric exact reference frame {:?}; workload INVALID",
                scenario.reference_symbol()
            )
        })?;
    if self_pct <= 0.0 {
        return Err("reference frame has zero self-time; workload INVALID".into());
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path, scenario: Scenario) -> Result<f64, String> {
    let mut samples = Vec::with_capacity(PROFILE_TRIALS);
    for trial in 1..=PROFILE_TRIALS {
        let self_pct = profile_trial(executable, scenario, trial)?;
        println!(
            "PROFILE_SELF scenario={} arm=reference trial={trial} self_pct={self_pct:.4}",
            scenario.name()
        );
        samples.push(self_pct);
    }
    let self_cv_pct = cv(&samples);
    let median_self_pct = median(&mut samples);
    println!(
        "PROFILE_SELF_SUMMARY scenario={} trials={PROFILE_TRIALS} median_self_pct={median_self_pct:.4} self_cv_pct={self_cv_pct:.4} samples={samples:?}",
        scenario.name()
    );
    Ok(median_self_pct)
}

fn perf_instructions(executable: &Path, scenario: Scenario, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args([
            "--child",
            scenario.name(),
            arm.name(),
            &scenario.stat_repeats().to_string(),
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

fn run_instruction_ab(executable: &Path, scenario: Scenario) -> Result<(), String> {
    for arm in [Arm::Reference, Arm::Candidate] {
        let status = Command::new(executable)
            .args(["--child", scenario.name(), arm.name(), "100"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
    }

    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    for round in 0..STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                Arm::Candidate
            } else {
                Arm::Reference
            };
            counts[slot] = perf_instructions(executable, scenario, arm)?;
        }
        let null = counts[0] as f64 / counts[1] as f64;
        let effect = counts[0] as f64 / counts[2] as f64;
        println!(
            "INSTRUCTIONS scenario={} round={} order={order:?} reference_a={} reference_b={} candidate={} null_ratio={null:.9} reference_over_candidate={effect:.9}",
            scenario.name(),
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        nulls.push(null);
        effects.push(effect);
    }

    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let (null_ci95_low, null_ci95_high) = bootstrap_median_ci(&nulls);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let null_ci_radius = (null_ci95_low - 1.0)
        .abs()
        .max((null_ci95_high - 1.0).abs());
    let decisive_threshold = 1.0 + 2.0 * null_ci_radius;
    let verdict = if effect_median >= decisive_threshold {
        "KEEP"
    } else if effect_median.recip() >= decisive_threshold {
        "REJECT"
    } else {
        "NULL"
    };
    println!(
        "INSTRUCTIONS_SUMMARY scenario={} rounds={STAT_ROUNDS} null_median={null_median:.9} null_ci95_low={null_ci95_low:.9} null_ci95_high={null_ci95_high:.9} bootstrap_resamples={BOOTSTRAP_RESAMPLES} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv_pct:.6} decisive_threshold={decisive_threshold:.9}",
        scenario.name()
    );
    println!(
        "DECISION scenario={} verdict={verdict} gate=median_ci_95 two_x_margin=true cv_used_as_provenance_only=true",
        scenario.name()
    );
    Ok(())
}

fn main() -> Result<(), String> {
    if let Some((scenario, arm, repeats)) = child_args()? {
        black_box(run_loop(scenario, arm, repeats));
        return Ok(());
    }

    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    let (sha256, bytes) = binary_identity(&executable)?;
    println!(
        "bench_elf_sha256={sha256} ({bytes} bytes) {}",
        executable.display()
    );
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("WORKER_ID {hostname}");

    for scenario in selected_scenarios()? {
        correctness_gate(scenario);
        run_profile(&executable, scenario)
            .map_err(|error| format!("PROFILE INVALID scenario={}: {error}", scenario.name()))?;
        run_instruction_ab(&executable, scenario)
            .map_err(|error| format!("A/B INVALID scenario={}: {error}", scenario.name()))?;
    }
    Ok(())
}
