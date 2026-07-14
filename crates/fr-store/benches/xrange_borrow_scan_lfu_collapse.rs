//! Same-binary A/B for the LFU XRANGE keyspace-probe collapse on the ZERO-COPY production path
//! (`xrange_borrow_scan`, the fr-runtime borrow-scan encoder for streams). The non-LFU path already
//! single-probes via `lookup_live_for_read_mut`; this folds the LFU path's `record_keyspace_lookup` +
//! `contains_key` rand-gate + `get_mut` into ONE `get_mut` (expiry peek + inline hit/miss +
//! `rand_sample` on a disjoint `&mut self.rng_seed` field split). 3 probes → 1. Byte/RNG/stat-
//! identical (`xrange_borrow_scan_lfu_collapsed_matches_threeprobe`).
//!
//! XRANGE is non-mutating → repeatable. Each timed op loops a full-range XRANGE over small streams,
//! summing field byte lengths through the sink. CAND = production `xrange_borrow_scan` (`::<true>`),
//! ORIG = `xrange_borrow_scan_lfu_threeprobe_bench`.

use std::hint::black_box;
use std::path::Path;
use std::process::{self, Command};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs};

use fr_store::{MaxmemoryPolicy, Store, StreamEntries, XrangeReplyEvent};

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.04;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

const KEYSPACE: usize = 50_000;
const MIN: (u64, u64) = (0, 0);
const MAX: (u64, u64) = (u64::MAX, u64::MAX);
const TAIL_PROFILE_ENTRIES: usize = 10_000;
const TAIL_PROFILE_START: (u64, u64) = (1, 9_901);
const TAIL_PROFILE_COUNT: usize = 8;
const TAIL_PROFILE_REPEATS: usize = 1_000_000;
const TAIL_STAT_REPEATS: usize = 200_000;
const TAIL_STAT_ROUNDS: usize = 21;

#[derive(Clone, Copy)]
enum TailRangeArm {
    Candidate,
    Reference,
}

impl TailRangeArm {
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
            _ => Err(format!("unknown tail-range arm {value:?}")),
        }
    }
}

fn build_tail_profile_entries() -> StreamEntries {
    let mut entries = StreamEntries::new();
    let fields = [(b"field".to_vec(), b"value".to_vec())];
    for sequence in 1..=TAIL_PROFILE_ENTRIES {
        assert!(!entries.insert((1, sequence as u64), &fields));
    }
    entries
}

#[inline(never)]
fn profile_tail_range_current(entries: &StreamEntries) -> u64 {
    let mut checksum = 0_u64;
    for (id, fields) in entries
        .range(black_box(TAIL_PROFILE_START)..=black_box(MAX))
        .take(black_box(TAIL_PROFILE_COUNT))
    {
        checksum = checksum.wrapping_add(id.1);
        for (field, value) in fields.iter() {
            checksum = checksum.wrapping_add((field.len() + value.len()) as u64);
        }
    }
    black_box(checksum)
}

#[inline(never)]
fn tail_range_candidate(entries: &StreamEntries) -> u64 {
    profile_tail_range_current(entries)
}

#[inline(never)]
fn tail_range_reference(entries: &StreamEntries) -> u64 {
    let mut checksum = 0_u64;
    for (id, fields) in entries
        .bench_range_completed_node_reference(black_box(TAIL_PROFILE_START)..=black_box(MAX))
        .take(black_box(TAIL_PROFILE_COUNT))
    {
        checksum = checksum.wrapping_add(id.1);
        for (field, value) in fields.iter() {
            checksum = checksum.wrapping_add((field.len() + value.len()) as u64);
        }
    }
    black_box(checksum)
}

fn run_tail_range_loop(arm: TailRangeArm, repeats: usize) {
    let entries = build_tail_profile_entries();
    let operation: fn(&StreamEntries) -> u64 = match arm {
        TailRangeArm::Candidate => tail_range_candidate,
        TailRangeArm::Reference => tail_range_reference,
    };
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        checksum = checksum.wrapping_add(operation(black_box(&entries)));
    }
    black_box(checksum);
}

fn tail_range_child_args() -> Result<Option<(TailRangeArm, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--tail-range-child") {
        return Ok(None);
    }
    let arm = TailRangeArm::parse(args.get(2).ok_or("missing tail-range child arm")?)?;
    let repeats = args
        .get(3)
        .ok_or("missing tail-range repeat count")?
        .parse()
        .map_err(|error| format!("invalid tail-range repeat count: {error}"))?;
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

fn profile_self_pct(report: &str, needle: &str) -> Option<f64> {
    report
        .lines()
        .find(|line| line.contains(needle))?
        .split_whitespace()
        .next()?
        .strip_suffix('%')?
        .parse()
        .ok()
}

fn run_tail_profile_if_requested() -> Result<bool, String> {
    if env::var_os("XRANGE_TAIL_PROFILE_CHILD").is_some() {
        let entries = build_tail_profile_entries();
        let mut checksum = 0_u64;
        for _ in 0..TAIL_PROFILE_REPEATS {
            checksum = checksum.wrapping_add(profile_tail_range_current(black_box(&entries)));
        }
        black_box(checksum);
        return Ok(true);
    }
    if !env::args().any(|arg| arg == "--profile-tail-range") {
        return Ok(false);
    }

    let executable = env::current_exe().map_err(|error| format!("current executable: {error}"))?;
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_xrange_tail_profile_{}_{}.data",
        process::id(),
        stamp
    ));
    if fs::exists(&data).map_err(|error| format!("inspect {}: {error}", data.display()))? {
        return Err(format!("refusing to overwrite {}", data.display()));
    }

    println!("WORKER_ID {hostname}");
    println!("BINARY_SHA256 current={}", binary_sha256(&executable)?);
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
        .arg(&executable)
        .env("XRANGE_TAIL_PROFILE_CHILD", "1")
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
            "--sort=symbol",
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
    println!("PROFILE_TABLE_BEGIN\n{stdout}\nPROFILE_TABLE_END");
    let self_pct = profile_self_pct(&stdout, "profile_tail_range_current")
        .ok_or("profile has no exact tail-range wrapper frame; workload INVALID")?;
    if self_pct <= 0.0 {
        return Err("exact tail-range wrapper has zero self-time; workload INVALID".to_owned());
    }
    println!(
        "PROFILE_SELF function=profile_tail_range_current self_pct={self_pct:.4} trigger=tail_first count={TAIL_PROFILE_COUNT} entries={TAIL_PROFILE_ENTRIES} repeats={TAIL_PROFILE_REPEATS}"
    );
    Ok(true)
}

fn perf_instructions(executable: &Path, arm: TailRangeArm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args([
            "--tail-range-child",
            arm.name(),
            &TAIL_STAT_REPEATS.to_string(),
        ])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat failed for {}: {stderr}", arm.name()));
    }
    stderr
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split(',').collect();
            fields
                .iter()
                .any(|field| field.trim().contains("instructions"))
                .then(|| fields.first()?.trim().parse().ok())
                .flatten()
        })
        .ok_or_else(|| format!("missing instruction count for {}: {stderr}", arm.name()))
}

fn measure_tail_pair(
    executable: &Path,
    baseline: TailRangeArm,
    changed: TailRangeArm,
    swapped: bool,
) -> Result<f64, String> {
    let (baseline_count, changed_count) = if swapped {
        let changed_count = perf_instructions(executable, changed)?;
        let baseline_count = perf_instructions(executable, baseline)?;
        (baseline_count, changed_count)
    } else {
        let baseline_count = perf_instructions(executable, baseline)?;
        let changed_count = perf_instructions(executable, changed)?;
        (baseline_count, changed_count)
    };
    Ok(baseline_count as f64 / changed_count as f64)
}

fn run_tail_range_ab_if_requested() -> Result<bool, String> {
    if !env::args().any(|arg| arg == "--tail-range") {
        return Ok(false);
    }

    let entries = build_tail_profile_entries();
    let candidate = entries
        .range(TAIL_PROFILE_START..=MAX)
        .take(TAIL_PROFILE_COUNT)
        .map(|(id, fields)| (*id, fields.to_pairs()))
        .collect::<Vec<_>>();
    let reference = entries
        .bench_range_completed_node_reference(TAIL_PROFILE_START..=MAX)
        .take(TAIL_PROFILE_COUNT)
        .map(|(id, fields)| (*id, fields.to_pairs()))
        .collect::<Vec<_>>();
    if candidate != reference {
        return Err("tail-range candidate/reference output mismatch".to_owned());
    }

    let executable = env::current_exe().map_err(|error| format!("current executable: {error}"))?;
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("WORKER_ID {hostname}");
    println!("BINARY_SHA256 both_arms={}", binary_sha256(&executable)?);

    for arm in [TailRangeArm::Reference, TailRangeArm::Candidate] {
        let status = Command::new(&executable)
            .args(["--tail-range-child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !status.success() {
            return Err(format!("{} warm-up failed with {status}", arm.name()));
        }
    }

    let mut nulls = Vec::with_capacity(TAIL_STAT_ROUNDS);
    let mut effects = Vec::with_capacity(TAIL_STAT_ROUNDS);
    for round in 0..=TAIL_STAT_ROUNDS {
        let swapped = round % 2 == 1;
        let null = measure_tail_pair(
            &executable,
            TailRangeArm::Reference,
            TailRangeArm::Reference,
            swapped,
        )?;
        let effect = measure_tail_pair(
            &executable,
            TailRangeArm::Reference,
            TailRangeArm::Candidate,
            swapped,
        )?;
        if round != 0 {
            nulls.push(null);
            effects.push(effect);
        }
    }

    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let lo = pct(&nulls, NULL_LO);
    let hi = pct(&nulls, NULL_HI);
    let verdict = if effect_median > 1.0 && effect_median > hi {
        "WIN(tail-direct)"
    } else if effect_median < 1.0 && effect_median < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "\n{:<24} {:>8} {:>9} {:>16} {:>8} {:>10} {:>12} {:>18}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "effect cv%", "ref/direct", "verdict"
    );
    println!(
        "{:<24} {:>8} {:>9.6} {:>16} {:>8.4} {:>10.4} {:>11.6}x {:>18}",
        "xrange_tail_count8",
        TAIL_STAT_REPEATS,
        null_median,
        format!("[{lo:.6}, {hi:.6}]"),
        cv(&nulls),
        cv(&effects),
        effect_median,
        verdict
    );
    Ok(true)
}

fn build() -> Store {
    let mut s = Store::new();
    s.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    s.lfu_decay_time = 0;
    for i in 0..KEYSPACE {
        let k = format!("k{i:08}").into_bytes();
        for j in 1..=3u64 {
            s.xadd(&k, (j, 0), &[(b"f".to_vec(), b"v".to_vec())], 1)
                .unwrap();
        }
    }
    s
}

fn scan_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("k{:08}", i * (KEYSPACE / n.max(1))).into_bytes())
        .collect()
}

#[inline(never)]
fn run_threeprobe(s: &mut Store, keys: &[&[u8]]) -> usize {
    let mut acc = 0usize;
    for &k in keys {
        let mut local = 0usize;
        let _ = s.xrange_borrow_scan_lfu_threeprobe_bench(k, MIN, MAX, None, 1, false, |ev| {
            if let XrangeReplyEvent::Field(f) = ev {
                local = local.wrapping_add(f.len());
            }
        });
        acc = acc.wrapping_add(local);
    }
    acc
}

#[inline(never)]
fn run_collapse(s: &mut Store, keys: &[&[u8]]) -> usize {
    let mut acc = 0usize;
    for &k in keys {
        let mut local = 0usize;
        let _ = s.xrange_borrow_scan(k, MIN, MAX, None, 1, false, |ev| {
            if let XrangeReplyEvent::Field(f) = ev {
                local = local.wrapping_add(f.len());
            }
        });
        acc = acc.wrapping_add(local);
    }
    acc
}

fn time(reps: usize, s: &mut Store, f: fn(&mut Store, &[&[u8]]) -> usize, keys: &[&[u8]]) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(s), black_box(keys)));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

fn median(r: &mut [f64]) -> f64 {
    r.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    r[r.len() / 2]
}

fn cv(r: &[f64]) -> f64 {
    let m = r.iter().sum::<f64>() / r.len() as f64;
    100.0 * (r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / r.len() as f64).sqrt() / m
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn bench(label: &str, s: &mut Store, n: usize) {
    let owned = scan_keys(n);
    let keys: Vec<&[u8]> = owned.iter().map(|k| k.as_slice()).collect();

    let mut reps = 1usize;
    loop {
        let e = time(reps, s, run_threeprobe, &keys);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 20 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let mut pair = |bf: fn(&mut Store, &[&[u8]]) -> usize,
                        cf: fn(&mut Store, &[&[u8]]) -> usize| {
            if swap {
                let c = time(reps, s, cf, &keys);
                time(reps, s, bf, &keys) / c
            } else {
                let b = time(reps, s, bf, &keys);
                b / time(reps, s, cf, &keys)
            }
        };
        let nn = pair(run_threeprobe, run_threeprobe);
        let sp = pair(run_threeprobe, run_collapse);
        if round == 0 {
            continue;
        }
        nulls.push(nn);
        speeds.push(sp);
    }

    let null_med = median(&mut nulls);
    let speedup = median(&mut speeds);
    let lo = pct(&nulls, NULL_LO);
    let hi = pct(&nulls, NULL_HI);
    let verdict = if speedup > 1.0 && speedup > hi {
        "WIN"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "{:<10} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
        label,
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}

fn main() {
    if let Some((arm, repeats)) = tail_range_child_args().expect("invalid tail-range child args") {
        run_tail_range_loop(arm, repeats);
        return;
    }
    if run_tail_profile_if_requested().expect("tail-range profile failed") {
        return;
    }
    if run_tail_range_ab_if_requested().expect("tail-range A/B failed") {
        return;
    }
    println!(
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_xrange", "reps", "NULL med", "null p5..p95", "null cv%", "collapse/3p", "verdict"
    );
    let mut s = build();
    bench("n32", &mut s, 32);
    bench("n256", &mut s, 256);
}
