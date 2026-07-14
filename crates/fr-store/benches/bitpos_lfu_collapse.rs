//! Same-binary A/B for the LFU BITPOS keyspace-probe collapse. Under allkeys-lfu the prior BITPOS
//! did two `entries` probes: a `contains_key` random-sample gate followed by `get_mut`. The candidate
//! relocates the identical RNG draw inside the successful `get_mut` borrow, reducing the hit path to
//! one probe. The two-byte search keeps BITPOS's scan work small enough to expose the probe cost.
//!
//! Both arms execute interleaved in this binary over the same present-key workload. Each measured
//! round is position-balanced, and the A/A control supplies the per-size null band.

use std::hint::black_box;
use std::process::Command;
use std::time::Instant;

use fr_store::{BitRangeUnit, MaxmemoryPolicy, Store};

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.04;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEYSPACE: usize = 50_000;

fn build() -> Store {
    let mut store = Store::new();
    store.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    store.lfu_decay_time = 0;
    for i in 0..KEYSPACE {
        store.set(format!("k{i:08}").into_bytes(), vec![0x00, 0x80], None, 1);
    }
    store
}

fn bitpos_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("k{:08}", i * (KEYSPACE / n.max(1))).into_bytes())
        .collect()
}

#[inline(never)]
fn run_twoprobe(store: &mut Store, keys: &[&[u8]]) -> u64 {
    let mut acc = 0u64;
    for &key in keys {
        let result = store.bitpos_lfu_twoprobe_bench(
            black_box(key),
            black_box(true),
            black_box(None),
            black_box(None),
            black_box(BitRangeUnit::Byte),
            black_box(1),
        );
        acc = acc.wrapping_add(match black_box(result) {
            Ok(position) => position as u64,
            Err(_) => 0xd1b5_4a32_d192_ed03,
        });
    }
    black_box(acc)
}

#[inline(never)]
fn run_collapse(store: &mut Store, keys: &[&[u8]]) -> u64 {
    let mut acc = 0u64;
    for &key in keys {
        let result = store.bitpos(
            black_box(key),
            black_box(true),
            black_box(None),
            black_box(None),
            black_box(BitRangeUnit::Byte),
            black_box(1),
        );
        acc = acc.wrapping_add(match black_box(result) {
            Ok(position) => position as u64,
            Err(_) => 0xd1b5_4a32_d192_ed03,
        });
    }
    black_box(acc)
}

type Arm = fn(&mut Store, &[&[u8]]) -> u64;

fn time(reps: usize, store: &mut Store, arm: Arm, keys: &[&[u8]]) -> f64 {
    let start = Instant::now();
    let mut acc = 0u64;
    for _ in 0..reps {
        acc = acc.wrapping_add(arm(black_box(store), black_box(keys)));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    values[values.len() / 2]
}

fn cv(values: &[f64]) -> f64 {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    100.0
        * (values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / values.len() as f64)
            .sqrt()
        / mean
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn profile_orig_child() {
    let mut store = build();
    let owned = bitpos_keys(256);
    let keys: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let mut acc = 0u64;
    for _ in 0..131_072 {
        acc = acc.wrapping_add(run_twoprobe(black_box(&mut store), black_box(&keys)));
    }
    black_box(acc);
}

fn run_profile_if_requested() -> bool {
    if std::env::var_os("BITPOS_PROFILE_CHILD").is_some() {
        profile_orig_child();
        return true;
    }
    if std::env::var_os("BITPOS_PROFILE").is_none() {
        return false;
    }

    let exe = std::env::current_exe().expect("current benchmark executable");
    let data = "/tmp/bitpos_lfu_collapse.perf.data";
    let status = Command::new("perf")
        .args([
            "record", "-q", "-e", "cycles:u", "-F", "999", "-o", data, "--",
        ])
        .arg(exe)
        .env("BITPOS_PROFILE_CHILD", "1")
        .status()
        .expect("run perf record");
    assert!(status.success(), "perf record failed: {status}");

    let report = Command::new("perf")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--sort=symbol",
            "--percent-limit=0.1",
            "-i",
            data,
        ])
        .output()
        .expect("run perf report");
    assert!(
        report.status.success(),
        "perf report failed: {}",
        report.status
    );
    print!("{}", String::from_utf8_lossy(&report.stdout));
    true
}

fn bench(label: &str, store: &mut Store, n: usize) {
    let owned = bitpos_keys(n);
    let keys: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();

    let mut reps = 1usize;
    loop {
        let elapsed = time(reps, store, run_twoprobe, &keys);
        if elapsed >= TARGET_SEGMENT_SECS || reps > 1 << 20 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / elapsed.max(1e-9)).max(1.0)).ceil()
                as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speedups = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let mut pair = |baseline: Arm, candidate: Arm| {
            if swap {
                let candidate_secs = time(reps, store, candidate, &keys);
                time(reps, store, baseline, &keys) / candidate_secs
            } else {
                let baseline_secs = time(reps, store, baseline, &keys);
                baseline_secs / time(reps, store, candidate, &keys)
            }
        };
        let null = pair(run_twoprobe, run_twoprobe);
        let speedup = pair(run_twoprobe, run_collapse);
        if round == 0 {
            continue;
        }
        nulls.push(null);
        speedups.push(speedup);
    }

    let null_median = median(&mut nulls);
    let speedup = median(&mut speedups);
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
        "{label:<10} {reps:>7} {null_median:>9.4} {:>16} {:>8.2} {speedup:>10.3}x {verdict:>16}",
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
    );
}

fn main() {
    if run_profile_if_requested() {
        return;
    }
    println!(
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_bitpos", "reps", "NULL med", "null p5..p95", "null cv%", "collapse/2p", "verdict"
    );
    let mut store = build();
    bench("n32", &mut store, 32);
    bench("n256", &mut store, 256);
}
