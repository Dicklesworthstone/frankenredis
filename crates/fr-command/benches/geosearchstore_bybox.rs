//! Profile-first, same-binary A/B for GEOSEARCHSTORE BYBOX bbox+borrow scanning.
//!
//! The unchanged production handler was profiled before the candidate existed.
//! This final harness keeps that exact materialize-and-scan path behind a bench-
//! only selector, proves full command/state identity, profiles both live arms,
//! and gates position-balanced instruction medians against a candidate A/A null.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_command::{bench_select_geosearchstore_bybox_reference, dispatch_argv, geo_encode_wgs84};
use fr_protocol::RespFrame;
use fr_store::Store;

const MEMBER_COUNT: usize = 32_768;
const PROFILE_REFERENCE_REPEATS: usize = 256;
const PROFILE_CANDIDATE_REPEATS: usize = 50_000;
const PROFILE_TRIALS: usize = 3;
const STAT_REPEATS: usize = 128;
const STAT_ROUNDS: usize = 24;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

#[derive(Clone, Copy, Debug)]
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
            _ => Err(format!("unknown child arm {value:?}")),
        }
    }

    const fn is_reference(self) -> bool {
        matches!(self, Self::Reference)
    }

    const fn profile_repeats(self) -> usize {
        match self {
            Self::Candidate => PROFILE_CANDIDATE_REPEATS,
            Self::Reference => PROFILE_REFERENCE_REPEATS,
        }
    }
}

fn unit(state: &mut u64) -> f64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state >> 11) as f64 / (1_u64 << 53) as f64
}

fn seed_store() -> Store {
    let mut state = 0x4753_5354_4f52_4542_u64;
    let mut members = Vec::with_capacity(MEMBER_COUNT);
    for i in 0..MEMBER_COUNT {
        let (lon, lat) = if i == 0 {
            (10.0, 40.0)
        } else {
            (
                unit(&mut state) * 360.0 - 180.0,
                unit(&mut state) * 170.0 - 85.0,
            )
        };
        let bits = geo_encode_wgs84(lon, lat).expect("generated WGS84 coordinate");
        let score = match i {
            1 => -1.0,
            2 => bits as f64 + 0.5,
            3 => (1_u64 << 53) as f64,
            4 => f64::INFINITY,
            _ => bits as f64,
        };
        let mut member = vec![b'x'; 64];
        let tag = format!("member:{i:08x}");
        member[..tag.len()].copy_from_slice(tag.as_bytes());
        members.push((score, member));
    }
    let mut store = Store::new();
    assert_eq!(
        store
            .zadd_plain_owned(b"geo:source", members, 1)
            .expect("seed GEO sorted set"),
        MEMBER_COUNT
    );
    store
}

fn argv() -> Vec<Vec<u8>> {
    [
        "GEOSEARCHSTORE",
        "geo:dest",
        "geo:source",
        "FROMLONLAT",
        "10",
        "40",
        "BYBOX",
        "40",
        "40",
        "km",
    ]
    .into_iter()
    .map(|arg| arg.as_bytes().to_vec())
    .collect()
}

fn run_live_path(arm: Arm, repeats: usize) {
    bench_select_geosearchstore_bybox_reference(arm.is_reference());
    let mut store = seed_store();
    let command = argv();
    let mut checksum = 0_i64;
    for _ in 0..repeats {
        let reply = dispatch_argv(black_box(&command), black_box(&mut store), 2)
            .expect("GEOSEARCHSTORE BYBOX dispatch");
        let RespFrame::Integer(count) = reply else {
            panic!("unexpected GEOSEARCHSTORE reply: {reply:?}");
        };
        checksum = checksum.wrapping_add(black_box(count));
    }
    black_box(checksum);
    black_box(
        store
            .zrange_withscores(b"geo:dest", 0, -1, 2)
            .expect("final destination read"),
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
        .ok_or_else(|| "missing child repeat count".to_owned())?
        .parse()
        .map_err(|error| format!("invalid child repeat count: {error}"))?;
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

fn self_pct(report: &str, needle: &str) -> f64 {
    report
        .lines()
        .filter(|line| line.contains(needle))
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|field| field.trim_end_matches('%').parse::<f64>().ok())
        .sum()
}

fn profile_trial(executable: &Path, arm: Arm, trial: usize) -> Result<(f64, f64), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_geosearchstore_bybox_{}_{}_{}_{}.data",
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
        .args(["--child", arm.name(), &arm.profile_repeats().to_string()])
        .output()
        .map_err(|error| format!("could not launch perf record: {error}"))?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let record_stderr = String::from_utf8_lossy(&recorded.stderr);
    if record_stderr.lines().any(|line| line.contains("lost ")) {
        return Err(format!(
            "perf record reported lost samples: {record_stderr}"
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
        "PROFILE_TABLE_BEGIN arm={} trial={trial}\n{stdout}\nPROFILE_TABLE_END arm={} trial={trial}",
        arm.name(),
        arm.name()
    );
    let lost_samples = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .and_then(|line| line.rsplit(':').next())
        .and_then(|field| field.trim().parse::<u64>().ok())
        .ok_or("perf report did not expose Total Lost Samples")?;
    if lost_samples != 0 {
        return Err(format!("perf report contains {lost_samples} lost samples"));
    }
    let handler_self_pct = self_pct(&stdout, "[.] fr_command::geosearchstore");
    let arm_helper = match arm {
        Arm::Candidate => "[.] fr_command::geo_searchstore_box_results",
        Arm::Reference => "[.] fr_command::geo_searchstore_box_reference",
    };
    let helper_self_pct = self_pct(&stdout, arm_helper);
    if helper_self_pct <= 0.0 {
        return Err(format!(
            "{} profile has zero exact helper self-time; workload is INVALID",
            arm.name()
        ));
    }
    Ok((handler_self_pct, helper_self_pct))
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| left.partial_cmp(right).expect("profile sample is not NaN"));
    samples[samples.len() / 2]
}

fn run_profile(executable: &Path) -> Result<(), String> {
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("WORKER_ID {hostname}");
    println!("BINARY_SHA256 same_binary={}", binary_sha256(executable)?);
    for arm in [Arm::Reference, Arm::Candidate] {
        let warm = Command::new(executable)
            .args(["--child", arm.name(), "8"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed with {warm}", arm.name()));
        }
    }
    for arm in [Arm::Reference, Arm::Candidate] {
        let mut handler_samples = Vec::with_capacity(PROFILE_TRIALS);
        let mut helper_samples = Vec::with_capacity(PROFILE_TRIALS);
        for trial in 1..=PROFILE_TRIALS {
            let (handler_self_pct, helper_self_pct) = profile_trial(executable, arm, trial)?;
            println!(
                "PROFILE_SELF arm={} trial={trial} geosearchstore_self_pct={handler_self_pct:.4} exact_helper_self_pct={helper_self_pct:.4} lost_samples=0",
                arm.name()
            );
            handler_samples.push(handler_self_pct);
            helper_samples.push(helper_self_pct);
        }
        let median_handler_self_pct = median(&mut handler_samples);
        let median_helper_self_pct = median(&mut helper_samples);
        println!(
            "PROFILE_SELF_SUMMARY arm={} trials={PROFILE_TRIALS} median_geosearchstore_self_pct={median_handler_self_pct:.4} median_exact_helper_self_pct={median_helper_self_pct:.4} handler_samples={handler_samples:?} helper_samples={helper_samples:?} report_floor_pct=0.1000",
            arm.name()
        );
        if median_helper_self_pct <= 0.1 {
            return Err(format!(
                "{} exact-helper attribution floor failed: helper={median_helper_self_pct:.4}%",
                arm.name()
            ));
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct CommandSnapshot {
    reply: RespFrame,
    reply_bytes: Vec<u8>,
    destination_scores: Vec<(Vec<u8>, u64)>,
    destination_dump: Option<Vec<u8>>,
    destination_encoding: Option<&'static str>,
    destination_pttl: String,
    destination_modification_count: u64,
    dirty: u64,
    keyspace_hits: u64,
    keyspace_misses: u64,
    state_digest: String,
}

fn command(args: &[&str]) -> Vec<Vec<u8>> {
    args.iter().map(|arg| arg.as_bytes().to_vec()).collect()
}

fn command_snapshot(arm: Arm, argv: &[Vec<u8>], protocol: i64) -> CommandSnapshot {
    let mut store = seed_store();
    let dest = argv[1].clone();
    if dest.as_slice() != b"geo:source" {
        store.set(
            dest.clone(),
            b"pre-existing destination".to_vec(),
            Some(50_000),
            1,
        );
    }
    store.dispatch_client_ctx.resp_protocol_version = protocol;
    bench_select_geosearchstore_bybox_reference(arm.is_reference());
    let reply = dispatch_argv(argv, &mut store, 2).expect("correctness GEOSEARCHSTORE");
    let reply_bytes = reply.to_bytes();
    let destination_scores = store
        .zrange_withscores(&dest, 0, -1, 2)
        .expect("destination must be a zset or missing")
        .into_iter()
        .map(|(member, score)| (member, score.to_bits()))
        .collect();
    let destination_dump = store.dump_key(&dest, 2);
    let destination_encoding = store.object_encoding(&dest, 2);
    let destination_pttl = format!("{:?}", store.pttl_no_stats(&dest, 2));
    let destination_modification_count = store.key_modification_count(&dest, 2);
    let dirty = store.dirty;
    let keyspace_hits = store.stat_keyspace_hits;
    let keyspace_misses = store.stat_keyspace_misses;
    let state_digest = store.state_digest();
    CommandSnapshot {
        reply,
        reply_bytes,
        destination_scores,
        destination_dump,
        destination_encoding,
        destination_pttl,
        destination_modification_count,
        dirty,
        keyspace_hits,
        keyspace_misses,
        state_digest,
    }
}

fn correctness_gate() {
    let cases = [
        (
            "selective_unsorted",
            command(&[
                "GEOSEARCHSTORE",
                "geo:dest",
                "geo:source",
                "FROMLONLAT",
                "10",
                "40",
                "BYBOX",
                "40",
                "40",
                "km",
            ]),
        ),
        (
            "broad_asc_count",
            command(&[
                "GEOSEARCHSTORE",
                "geo:dest",
                "geo:source",
                "FROMLONLAT",
                "0",
                "0",
                "BYBOX",
                "40000",
                "20000",
                "km",
                "ASC",
                "COUNT",
                "64",
            ]),
        ),
        (
            "desc_count_storedist",
            command(&[
                "GEOSEARCHSTORE",
                "geo:dest",
                "geo:source",
                "FROMLONLAT",
                "10",
                "40",
                "BYBOX",
                "5000",
                "5000",
                "km",
                "DESC",
                "COUNT",
                "64",
                "STOREDIST",
            ]),
        ),
        (
            "count_any",
            command(&[
                "GEOSEARCHSTORE",
                "geo:dest",
                "geo:source",
                "FROMLONLAT",
                "10",
                "40",
                "BYBOX",
                "5000",
                "5000",
                "km",
                "COUNT",
                "8",
                "ANY",
            ]),
        ),
        (
            "antimeridian",
            command(&[
                "GEOSEARCHSTORE",
                "geo:dest",
                "geo:source",
                "FROMLONLAT",
                "179.9",
                "0",
                "BYBOX",
                "1000",
                "1000",
                "km",
            ]),
        ),
        (
            "high_latitude",
            command(&[
                "GEOSEARCHSTORE",
                "geo:dest",
                "geo:source",
                "FROMLONLAT",
                "30",
                "84",
                "BYBOX",
                "2000",
                "400",
                "km",
            ]),
        ),
        (
            "zero_box_deletes_destination",
            command(&[
                "GEOSEARCHSTORE",
                "geo:dest",
                "geo:source",
                "FROMLONLAT",
                "20",
                "20",
                "BYBOX",
                "0",
                "0",
                "m",
            ]),
        ),
        (
            "missing_source_deletes_destination",
            command(&[
                "GEOSEARCHSTORE",
                "geo:dest",
                "geo:missing",
                "FROMLONLAT",
                "10",
                "40",
                "BYBOX",
                "40",
                "40",
                "km",
            ]),
        ),
        (
            "destination_equals_source",
            command(&[
                "GEOSEARCHSTORE",
                "geo:source",
                "geo:source",
                "FROMLONLAT",
                "10",
                "40",
                "BYBOX",
                "40",
                "40",
                "km",
            ]),
        ),
    ];
    for protocol in [2, 3] {
        for (name, argv) in &cases {
            let reference = command_snapshot(Arm::Reference, argv, protocol);
            let candidate = command_snapshot(Arm::Candidate, argv, protocol);
            assert_eq!(
                candidate, reference,
                "full GEOSEARCHSTORE mismatch case={name} RESP{protocol}"
            );
        }
    }
    println!(
        "CORRECTNESS_GATE cases={} protocols=RESP2,RESP3 reply_bytes_scores_bits_dump_encoding_ttl_dirty_modcount_stats_digest=identical",
        cases.len()
    );
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat for {}: {error}", arm.name()))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat for {} failed: {stderr}", arm.name()));
    }
    for line in stderr.lines() {
        let columns: Vec<_> = line.split(',').collect();
        if columns
            .iter()
            .any(|field| field.trim().contains("instructions"))
        {
            let raw = columns[0].trim();
            if raw.starts_with('<') {
                return Err(format!("perf counter unavailable: {line}"));
            }
            return raw
                .parse()
                .map_err(|error| format!("invalid perf count {raw:?}: {error}"));
        }
    }
    Err(format!("instructions:u missing from perf output: {stderr}"))
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

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut null_ratios = Vec::with_capacity(STAT_ROUNDS);
    let mut speedups = Vec::with_capacity(STAT_ROUNDS);
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
        let null_ratio = counts[0] as f64 / counts[1] as f64;
        let speedup = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} null_ratio={null_ratio:.9} reference_over_candidate={speedup:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        null_ratios.push(null_ratio);
        speedups.push(speedup);
    }

    let null_cv_pct = cv(&null_ratios);
    let speedup_cv_pct = cv(&speedups);
    let null_median = median(&mut null_ratios);
    let speedup_median = median(&mut speedups);
    let null_p05 = percentile(&null_ratios, NULL_LO);
    let null_p95 = percentile(&null_ratios, NULL_HI);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={speedup_median:.9} speedup_cv_pct={speedup_cv_pct:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!(
            "null median exposes harness bias: {null_median:.9}"
        ));
    }
    if speedup_median <= null_p95 {
        return Err(format!(
            "candidate median does not clear null spread: speedup={speedup_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    if speedup_median <= 1.01 {
        return Err(format!(
            "1% instruction keep gate failed: {speedup_median:.9}x"
        ));
    }
    Ok(())
}

fn main() {
    match child_args() {
        Ok(Some((arm, repeats))) => {
            run_live_path(arm, repeats);
            return;
        }
        Ok(None) => {}
        Err(error) => panic!("invalid child arguments: {error}"),
    }
    let executable = env::current_exe().expect("current bench executable path");
    correctness_gate();
    run_profile(&executable).unwrap_or_else(|error| panic!("PROFILE INVALID: {error}"));
    run_instruction_ab(&executable).unwrap_or_else(|error| panic!("A/B INVALID: {error}"));
}
