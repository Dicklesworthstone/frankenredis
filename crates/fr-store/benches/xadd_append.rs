//! Same-binary profile and A/B harness for XADD's grouped stream-node directory.
//!
//! The candidate keeps the mutable rightmost node outside the B-tree; the
//! reference freezes the exact pre-change all-nodes-in-B-tree implementation.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_store::{PackedStreamLogBTreeReference, StreamEntries};

const SEED_ENTRIES: usize = 100;
const PROFILE_REPEATS: usize = 2_000_000;
const PROFILE_TRIALS: usize = 5;
const STAT_REPEATS: usize = 20_000;
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
}

#[inline(never)]
fn insert_candidate(
    entries: &mut StreamEntries,
    id: (u64, u64),
    fields: &[(Vec<u8>, Vec<u8>)],
) -> bool {
    entries.insert(black_box(id), black_box(fields))
}

#[inline(never)]
fn insert_reference(
    entries: &mut PackedStreamLogBTreeReference,
    id: (u64, u64),
    fields: &[(Vec<u8>, Vec<u8>)],
) -> bool {
    entries.insert(black_box(id), black_box(fields))
}

fn seed_candidate() -> StreamEntries {
    let mut entries = StreamEntries::new();
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for sequence in 1..=SEED_ENTRIES {
        assert!(!insert_candidate(
            &mut entries,
            (1, sequence as u64),
            &fields
        ));
    }
    entries
}

fn seed_reference() -> PackedStreamLogBTreeReference {
    let mut entries = PackedStreamLogBTreeReference::new();
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for sequence in 1..=SEED_ENTRIES {
        assert!(!insert_reference(
            &mut entries,
            (1, sequence as u64),
            &fields
        ));
    }
    entries
}

fn run_candidate_loop(repeats: usize) {
    let mut entries = seed_candidate();
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for offset in 1..=repeats {
        black_box(insert_candidate(&mut entries, (2, offset as u64), &fields));
    }
    black_box(entries.len());
    black_box(entries.last_id());
    std::mem::forget(entries);
}

fn run_reference_loop(repeats: usize) {
    let mut entries = seed_reference();
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for offset in 1..=repeats {
        black_box(insert_reference(&mut entries, (2, offset as u64), &fields));
    }
    black_box(entries.len());
    black_box(entries.last_id());
    std::mem::forget(entries);
}

fn run_append_loop(arm: Arm, repeats: usize) {
    match arm {
        Arm::Candidate => run_candidate_loop(repeats),
        Arm::Reference => run_reference_loop(repeats),
    }
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
    candidate_wrapper_self_pct: f64,
    reference_wrapper_self_pct: f64,
    reference_insert_self_pct: f64,
    btree_insert_self_pct: f64,
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
        candidate_wrapper_self_pct: self_pct(stdout.as_ref(), "xadd_append::insert_candidate"),
        reference_wrapper_self_pct: self_pct(stdout.as_ref(), "xadd_append::insert_reference"),
        reference_insert_self_pct: self_pct(
            stdout.as_ref(),
            "<fr_store::packed_set::PackedStreamLogBTreeReference>::insert",
        ),
        btree_insert_self_pct: self_pct(
            stdout.as_ref(),
            "BTreeMap<(u64, u64), fr_store::packed_set::StreamNode>>::insert",
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

    for arm in [Arm::Reference, Arm::Candidate] {
        let warm = Command::new(executable)
            .args(["--child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed with {warm}", arm.name()));
        }
    }

    let mut reference_wrapper = Vec::with_capacity(PROFILE_TRIALS);
    let mut reference_insert = Vec::with_capacity(PROFILE_TRIALS);
    let mut reference_btree_insert = Vec::with_capacity(PROFILE_TRIALS);
    let mut candidate_wrapper = Vec::with_capacity(PROFILE_TRIALS);
    let mut candidate_btree_insert = Vec::with_capacity(PROFILE_TRIALS);
    for arm in [Arm::Reference, Arm::Candidate] {
        for trial in 1..=PROFILE_TRIALS {
            let rows = profile_trial(executable, arm, trial)?;
            println!(
                "PROFILE_SELF arm={} trial={trial} candidate_wrapper_self_pct={:.4} \
reference_wrapper_self_pct={:.4} reference_insert_self_pct={:.4} btree_insert_self_pct={:.4}",
                arm.name(),
                rows.candidate_wrapper_self_pct,
                rows.reference_wrapper_self_pct,
                rows.reference_insert_self_pct,
                rows.btree_insert_self_pct
            );
            match arm {
                Arm::Reference => {
                    reference_wrapper.push(rows.reference_wrapper_self_pct);
                    reference_insert.push(rows.reference_insert_self_pct);
                    reference_btree_insert.push(rows.btree_insert_self_pct);
                }
                Arm::Candidate if rows.reference_wrapper_self_pct > 0.1 => {
                    return Err(format!(
                        "candidate unexpectedly reaches the frozen reference wrapper: {rows:?}"
                    ));
                }
                Arm::Candidate => {
                    candidate_wrapper.push(rows.candidate_wrapper_self_pct);
                    candidate_btree_insert.push(rows.btree_insert_self_pct);
                }
            }
        }
    }
    let reference_wrapper_median = median(&mut reference_wrapper);
    let reference_insert_median = median(&mut reference_insert);
    let reference_btree_insert_median = median(&mut reference_btree_insert);
    let candidate_wrapper_median = median(&mut candidate_wrapper);
    let candidate_btree_insert_median = median(&mut candidate_btree_insert);
    println!(
        "PROFILE_SELF_SUMMARY trials={PROFILE_TRIALS} \
reference_wrapper_median_self_pct={reference_wrapper_median:.4} reference_wrapper_samples={reference_wrapper:?} \
reference_insert_median_self_pct={reference_insert_median:.4} reference_insert_samples={reference_insert:?} \
reference_btree_insert_median_self_pct={reference_btree_insert_median:.4} reference_btree_insert_samples={reference_btree_insert:?} \
candidate_wrapper_median_self_pct={candidate_wrapper_median:.4} candidate_wrapper_samples={candidate_wrapper:?} \
candidate_btree_insert_median_self_pct={candidate_btree_insert_median:.4} candidate_btree_insert_samples={candidate_btree_insert:?} \
report_floor_pct=0.1000"
    );
    if reference_wrapper_median <= 0.1 {
        return Err(format!(
            "median reference wrapper self-time {reference_wrapper_median:.4}% does not clear the 0.1% execution floor"
        ));
    }
    if candidate_wrapper_median <= 0.1 {
        return Err(format!(
            "median candidate wrapper self-time {candidate_wrapper_median:.4}% does not clear the 0.1% execution floor"
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
    let mut candidate = seed_candidate();
    let mut reference = seed_reference();
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for offset in 1..=250_u64 {
        assert_eq!(
            insert_candidate(&mut candidate, (2, offset * 2), &fields),
            insert_reference(&mut reference, (2, offset * 2), &fields)
        );
    }
    let overwrite = [(b"field".to_vec(), b"overwrite".to_vec())];
    assert_eq!(
        insert_candidate(&mut candidate, (2, 200), &overwrite),
        insert_reference(&mut reference, (2, 200), &overwrite)
    );
    let out_of_order = [(b"field".to_vec(), b"out-of-order".to_vec())];
    // Before the first node, inside a full middle node, and in an inter-node gap.
    for id in [(0, 1), (2, 199), (1, 10_000)] {
        assert_eq!(
            insert_candidate(&mut candidate, id, &out_of_order),
            insert_reference(&mut reference, id, &out_of_order)
        );
    }

    for id in [(0, 1), (2, 199), (2, 500)] {
        assert_eq!(candidate.remove(id), reference.remove(id));
    }

    let contents = |entries: &StreamEntries| {
        entries
            .iter()
            .map(|(id, fields)| (*id, fields.to_pairs()))
            .collect::<Vec<_>>()
    };
    let assert_same = |candidate: &StreamEntries, reference: &PackedStreamLogBTreeReference| {
        assert_eq!(candidate.len(), reference.len());
        assert_eq!(candidate.first_id(), reference.first_id());
        assert_eq!(candidate.last_id(), reference.last_id());
        assert_eq!(contents(candidate), reference.contents());
        assert_eq!(candidate.bench_node_layout(), reference.layout());
    };
    assert_same(&candidate, &reference);

    use std::ops::Bound::{Excluded, Included, Unbounded};
    let range_cases = [
        (Unbounded, Unbounded),
        (Included((0, 0)), Excluded((1, 0))),
        (Included((1, 50)), Included((2, 250))),
        (Excluded((1, 50)), Excluded((2, 250))),
        (Included((1, 10_001)), Included((2, 250))),
        (Excluded((3, 0)), Unbounded),
    ];
    for bounds in range_cases {
        assert_eq!(
            candidate
                .range(bounds)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            reference.range_ids(bounds)
        );
        let mut reference_reverse = reference.range_ids(bounds);
        reference_reverse.reverse();
        assert_eq!(
            candidate
                .range(bounds)
                .rev()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            reference_reverse
        );
    }

    let remaining_ids = candidate.keys().copied().collect::<Vec<_>>();
    for id in remaining_ids {
        assert_eq!(candidate.remove(id), reference.remove(id));
    }
    assert!(candidate.is_empty());
    assert!(reference.is_empty());
    assert_eq!(
        insert_candidate(&mut candidate, (9, 9), &fields),
        insert_reference(&mut reference, (9, 9), &fields)
    );
    assert_same(&candidate, &reference);
    println!(
        "CORRECTNESS_GATE append_overwrite_front_middle_gap_remove_empty_reinsert_ranges=identical"
    );
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
        let null_ratio = counts[1] as f64 / counts[0] as f64;
        let speedup = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} \
candidate_b_over_a={null_ratio:.9} reference_over_candidate={speedup:.9}",
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
reference_over_candidate_median={speedup_median:.9} speedup_cv_pct={speedup_cv_pct:.6}"
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
