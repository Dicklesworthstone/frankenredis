//! Same-binary A/B for the LFU ZREM keyspace-probe collapse. Under allkeys-lfu the prior path
//! used a `contains_key` random-sample gate before `get_mut`; the production candidate resolves
//! the entry once and draws through the disjoint `rng_seed` field only after a hit.
//!
//! Removing a same-length missing member is production-valid, repeatable, and non-destructive
//! while retaining the full ZREM lookup, LFU bump, packed-zset lookup, and result path. ORIG is
//! `zrem_lfu_twoprobe_bench`; CAND is production `zrem`.

use std::hint::black_box;
use std::time::Instant;

use fr_store::{MaxmemoryPolicy, Store};

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.04;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEYSPACE: usize = 50_000;
const MISSING_MEMBER: &[u8] = b"absent";

fn build() -> Store {
    let mut store = Store::new();
    store.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    store.lfu_decay_time = 0;
    for i in 0..KEYSPACE {
        let key = format!("k{i:08}").into_bytes();
        store
            .zadd_plain_owned(&key, vec![(1.0, b"stored".to_vec())], 1)
            .unwrap();
    }
    store
}

fn zrem_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("k{:08}", i * (KEYSPACE / n.max(1))).into_bytes())
        .collect()
}

#[inline(never)]
fn run_twoprobe(store: &mut Store, keys: &[&[u8]]) -> usize {
    let mut acc = 0usize;
    for &key in keys {
        if let Ok(removed) = store.zrem_lfu_twoprobe_bench(
            black_box(key),
            black_box(&[MISSING_MEMBER]),
            black_box(1),
        ) {
            acc = acc.wrapping_add(black_box(removed) as usize);
        }
    }
    acc
}

#[inline(never)]
fn run_collapse(store: &mut Store, keys: &[&[u8]]) -> usize {
    let mut acc = 0usize;
    for &key in keys {
        if let Ok(removed) = store.zrem(black_box(key), black_box(&[MISSING_MEMBER]), black_box(1))
        {
            acc = acc.wrapping_add(black_box(removed) as usize);
        }
    }
    acc
}

fn time(
    reps: usize,
    store: &mut Store,
    function: fn(&mut Store, &[&[u8]]) -> usize,
    keys: &[&[u8]],
) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        acc = acc.wrapping_add(function(black_box(store), black_box(keys)));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|left, right| left.partial_cmp(right).expect("no NaN"));
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

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

fn bench(label: &str, store: &mut Store, n: usize) {
    let owned = zrem_keys(n);
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
        let reverse = round % 2 == 1;
        let pair = |baseline: fn(&mut Store, &[&[u8]]) -> usize,
                    candidate: fn(&mut Store, &[&[u8]]) -> usize,
                    store: &mut Store| {
            if reverse {
                let candidate_secs = time(reps, store, candidate, &keys);
                time(reps, store, baseline, &keys) / candidate_secs
            } else {
                let baseline_secs = time(reps, store, baseline, &keys);
                baseline_secs / time(reps, store, candidate, &keys)
            }
        };
        let null = pair(run_twoprobe, run_twoprobe, store);
        let speedup = pair(run_twoprobe, run_collapse, store);
        if round == 0 {
            continue;
        }
        nulls.push(null);
        speedups.push(speedup);
    }

    let null_median = median(&mut nulls);
    let speedup = median(&mut speedups);
    let low = percentile(&nulls, NULL_LO);
    let high = percentile(&nulls, NULL_HI);
    let verdict = if speedup > 1.0 && speedup > high {
        "WIN"
    } else if speedup < 1.0 && speedup < low {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "{label:<10} {reps:>7} {null_median:>9.4} {:>16} {:>8.2} {speedup:>10.3}x {verdict:>16}",
        format!("[{low:.3}, {high:.3}]"),
        cv(&nulls),
    );
}

fn main() {
    println!(
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_zrem", "reps", "NULL med", "null p5..p95", "null cv%", "collapse/2p", "verdict"
    );
    let mut store = build();
    bench("n32", &mut store, 32);
    bench("n256", &mut store, 256);
}
