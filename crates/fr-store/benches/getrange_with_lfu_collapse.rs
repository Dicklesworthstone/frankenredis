//! Same-binary A/B for LFU GETRANGE's keyspace-probe collapse on the borrowing
//! `getrange_with` path used by the runtime's zero-copy encoder. The prior path did three probes
//! (`record_keyspace_lookup`, a `contains_key` rand gate, and `get_mut`); the candidate folds hit/
//! miss accounting into `get_mut` and draws from the disjoint `rng_seed` field — one probe.
//! Unlike allocating `getrange`, the timed closure only consumes the borrowed slice length.
//!
//! ORIG = `getrange_with_lfu_threeprobe_bench`; CAND =
//! `getrange_with_lfu_collapsed_bench`. Each measured pair is position-balanced and accompanied by
//! an A/A null pair in the same routine.

use std::hint::black_box;
use std::time::Instant;

use fr_store::{MaxmemoryPolicy, Store};

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
        store.set(format!("k{i:08}").into_bytes(), vec![b'x'; 32], None, 1);
    }
    store
}

fn getrange_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("k{:08}", i * (KEYSPACE / n.max(1))).into_bytes())
        .collect()
}

#[inline(never)]
fn run_threeprobe(store: &mut Store, keys: &[&[u8]]) -> usize {
    let mut acc = 0usize;
    for &key in keys {
        let result = store.getrange_with_lfu_threeprobe_bench(
            black_box(key),
            black_box(0),
            black_box(31),
            black_box(1),
            |slice| black_box(slice).len(),
        );
        if let Ok(len) = black_box(result) {
            acc = acc.wrapping_add(len);
        }
    }
    acc
}

#[inline(never)]
fn run_collapse(store: &mut Store, keys: &[&[u8]]) -> usize {
    let mut acc = 0usize;
    for &key in keys {
        let result = store.getrange_with_lfu_collapsed_bench(
            black_box(key),
            black_box(0),
            black_box(31),
            black_box(1),
            |slice| black_box(slice).len(),
        );
        if let Ok(len) = black_box(result) {
            acc = acc.wrapping_add(len);
        }
    }
    acc
}

fn time(
    reps: usize,
    store: &mut Store,
    f: fn(&mut Store, &[&[u8]]) -> usize,
    keys: &[&[u8]],
) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(store), black_box(keys)));
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
    100.0 * (values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
        / mean
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn bench(label: &str, store: &mut Store, n: usize) {
    let owned = getrange_keys(n);
    let keys: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();

    let mut reps = 1usize;
    loop {
        let elapsed = time(reps, store, run_threeprobe, &keys);
        if elapsed >= TARGET_SEGMENT_SECS || reps > 1 << 20 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / elapsed.max(1e-9)).max(1.0)).ceil()
                as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let mut pair = |baseline: fn(&mut Store, &[&[u8]]) -> usize,
                        candidate: fn(&mut Store, &[&[u8]]) -> usize| {
            if swap {
                let cand = time(reps, store, candidate, &keys);
                time(reps, store, baseline, &keys) / cand
            } else {
                let base = time(reps, store, baseline, &keys);
                base / time(reps, store, candidate, &keys)
            }
        };
        let null = pair(run_threeprobe, run_threeprobe);
        let speed = pair(run_threeprobe, run_collapse);
        if round == 0 {
            continue;
        }
        nulls.push(null);
        speeds.push(speed);
    }

    let null_median = median(&mut nulls);
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
        null_median,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}

fn main() {
    println!(
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_getrange", "reps", "NULL med", "null p5..p95", "null cv%", "collapse/3p", "verdict"
    );
    let mut store = build();
    bench("n32", &mut store, 32);
    bench("n256", &mut store, 256);
}
