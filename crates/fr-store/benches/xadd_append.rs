//! Same-binary profile and A/B harness for the monotonic stream-append path used by XADD.
//!
//! The candidate uses the last stream node to prove that a strictly increasing ID is new and to
//! append it. The fallback is the exact pre-change `node_key_for` + `insert_new_span` path.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_store::StreamEntries;

const SEED_ENTRIES: usize = 100;
const PROFILE_REPEATS: usize = 500_000;
const PROFILE_TRIALS: usize = 5;
const STAT_REPEATS: usize = 20_000;
const STAT_ROUNDS: usize = 24;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

#[derive(Clone, Copy, Debug)]
enum Arm {
    Candidate,
    Fallback,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Fallback => "fallback",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "fallback" => Ok(Self::Fallback),
            _ => Err(format!("unknown child arm {value:?}")),
        }
    }
}

fn insert(
    entries: &mut StreamEntries,
    arm: Arm,
    id: (u64, u64),
    fields: &[(Vec<u8>, Vec<u8>)],
) -> bool {
    match arm {
        Arm::Candidate => entries.insert(black_box(id), black_box(fields)),
        Arm::Fallback => entries.bench_insert_fallback(black_box(id), black_box(fields)),
    }
}

fn seed_entries(arm: Arm) -> StreamEntries {
    let mut entries = StreamEntries::new();
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for sequence in 1..=SEED_ENTRIES {
        assert!(!insert(&mut entries, arm, (1, sequence as u64), &fields));
    }
    entries
}

fn run_append_loop(arm: Arm, repeats: usize) {
    let mut entries = seed_entries(arm);
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for offset in 1..=repeats {
        black_box(insert(&mut entries, arm, (2, offset as u64), &fields));
    }
    black_box(entries.len());
    black_box(entries.last_id());
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

#[derive(Clone, Copy, Debug)]
struct ProfileRows {
    fallback_self_pct: f64,
    insert_new_span_self_pct: f64,
    node_key_for_self_pct: f64,
    btree_range_self_pct: f64,
}

fn self_pct(stdout: &str, needle: &str) -> f64 {
    stdout
        .lines()
        .filter(|line| line.contains(needle))
        .find_map(|line| {
            line.split_whitespace()
                .next()?
                .strip_suffix('%')?
                .parse()
                .ok()
        })
        .unwrap_or(0.0)
}

fn profile_trial(executable: &Path, arm: Arm, trial: usize) -> Result<ProfileRows, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_xadd_append_{}_{}_{}_{}.data",
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
    Ok(ProfileRows {
        fallback_self_pct: self_pct(stdout.as_ref(), "PackedStreamLog>::insert_span_fallback"),
        insert_new_span_self_pct: self_pct(stdout.as_ref(), "PackedStreamLog>::insert_new_span"),
        node_key_for_self_pct: self_pct(stdout.as_ref(), "PackedStreamLog>::node_key_for"),
        btree_range_self_pct: self_pct(
            stdout.as_ref(),
            "BTreeMap<(u64, u64), fr_store::packed_set::StreamNode>>::range",
        ),
    })
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| {
        left.partial_cmp(right)
            .expect("profile self-time is not NaN")
    });
    samples[samples.len() / 2]
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

    for arm in [Arm::Fallback, Arm::Candidate] {
        let warm = Command::new(executable)
            .args(["--child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed with {warm}", arm.name()));
        }
    }

    let mut fallback_core = Vec::with_capacity(PROFILE_TRIALS);
    let mut fallback_insert = Vec::with_capacity(PROFILE_TRIALS);
    let mut fallback_node_key = Vec::with_capacity(PROFILE_TRIALS);
    let mut fallback_range = Vec::with_capacity(PROFILE_TRIALS);
    for arm in [Arm::Fallback, Arm::Candidate] {
        for trial in 1..=PROFILE_TRIALS {
            let rows = profile_trial(executable, arm, trial)?;
            println!(
                "PROFILE_SELF arm={} trial={trial} fallback_self_pct={:.4} \
insert_new_span_self_pct={:.4} node_key_for_self_pct={:.4} btree_range_self_pct={:.4}",
                arm.name(),
                rows.fallback_self_pct,
                rows.insert_new_span_self_pct,
                rows.node_key_for_self_pct,
                rows.btree_range_self_pct
            );
            match arm {
                Arm::Fallback => {
                    fallback_core.push(rows.fallback_self_pct);
                    fallback_insert.push(rows.insert_new_span_self_pct);
                    fallback_node_key.push(rows.node_key_for_self_pct);
                    fallback_range.push(rows.btree_range_self_pct);
                }
                Arm::Candidate
                    if rows.fallback_self_pct > 0.1
                        || rows.insert_new_span_self_pct > 0.1
                        || rows.node_key_for_self_pct > 0.1 =>
                {
                    return Err(format!("candidate still reaches old lookup path: {rows:?}"));
                }
                Arm::Candidate => {}
            }
        }
    }
    let core_median = median(&mut fallback_core);
    let insert_median = median(&mut fallback_insert);
    let node_key_median = median(&mut fallback_node_key);
    let range_median = median(&mut fallback_range);
    println!(
        "PROFILE_SELF_SUMMARY arm=fallback trials={PROFILE_TRIALS} \
fallback_median_self_pct={core_median:.4} fallback_samples={fallback_core:?} \
insert_new_span_median_self_pct={insert_median:.4} insert_samples={fallback_insert:?} \
node_key_for_median_self_pct={node_key_median:.4} node_key_samples={fallback_node_key:?} \
btree_range_median_self_pct={range_median:.4} range_samples={fallback_range:?} \
candidate_fallback_reported_self_pct=0.0000 report_floor_pct=0.1000"
    );
    if core_median <= 0.1 {
        return Err(format!(
            "median fallback self-time {core_median:.4}% does not clear the 0.1% attribution floor"
        ));
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

fn correctness_gate() {
    let mut candidate = seed_entries(Arm::Candidate);
    let mut fallback = seed_entries(Arm::Fallback);
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for offset in 1..=250_u64 {
        assert_eq!(
            insert(&mut candidate, Arm::Candidate, (2, offset), &fields),
            insert(&mut fallback, Arm::Fallback, (2, offset), &fields)
        );
    }
    let overwrite = [(b"field".to_vec(), b"overwrite".to_vec())];
    assert_eq!(
        insert(&mut candidate, Arm::Candidate, (2, 100), &overwrite),
        insert(&mut fallback, Arm::Fallback, (2, 100), &overwrite)
    );
    let out_of_order = [(b"field".to_vec(), b"out-of-order".to_vec())];
    assert_eq!(
        insert(&mut candidate, Arm::Candidate, (1, 10_000), &out_of_order),
        insert(&mut fallback, Arm::Fallback, (1, 10_000), &out_of_order)
    );
    let contents = |entries: &StreamEntries| {
        entries
            .iter()
            .map(|(id, fields)| (*id, fields.to_pairs()))
            .collect::<Vec<_>>()
    };
    assert_eq!(candidate.len(), fallback.len());
    assert_eq!(candidate.first_id(), fallback.first_id());
    assert_eq!(candidate.last_id(), fallback.last_id());
    assert_eq!(contents(&candidate), contents(&fallback));
    println!("CORRECTNESS_GATE insert_results_len_ids_order_fields=identical");
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
                Arm::Fallback
            } else {
                Arm::Candidate
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null_ratio = counts[0] as f64 / counts[1] as f64;
        let speedup = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} fallback={} \
null_ratio={null_ratio:.9} fallback_over_candidate={speedup:.9}",
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
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} \
null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} \
fallback_over_candidate_median={speedup_median:.9} speedup_cv_pct={speedup_cv_pct:.6}"
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
            run_append_loop(arm, repeats);
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
