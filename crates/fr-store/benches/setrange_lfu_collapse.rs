//! Same-binary A/B for the LFU SETRANGE keyspace-probe collapse. Under allkeys-lfu the prior
//! SETRANGE did two `entries` probes: a `contains_key` LFU rand-gate + `with_mutated_entry`'s
//! `get_mut`. The collapse resolves the entry with a single `get_mut`-first probe and draws
//! `rand_sample` on a disjoint `&mut self.rng_seed` field split while the entry is held, inlining
//! `with_mutated_entry`'s exact digest bookkeeping — ONE probe. Byte/RNG/digest-identical
//! (`setrange_lfu_collapsed_matches_twoprobe`).
//!
//! SETRANGE overwriting 1 byte in-bounds is non-growing and repeatable, and the value is a borrowed
//! `&[u8]` (NO per-call alloc — unlike owned-value LSET which went sub-gate). Each timed op loops
//! single-byte SETRANGEs over a spread of small strings. CAND = production `setrange`
//! (`setrange_impl::<true>`), ORIG = `setrange_lfu_twoprobe_bench`.

use std::hint::black_box;
use std::time::Instant;

use fr_store::{MaxmemoryPolicy, Store};

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.04;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

const KEYSPACE: usize = 50_000;

fn build() -> Store {
    let mut s = Store::new();
    s.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    s.lfu_decay_time = 0;
    for i in 0..KEYSPACE {
        s.set(format!("k{i:08}").into_bytes(), b"abcdefgh".to_vec(), None, 1);
    }
    s
}

fn setrange_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("k{:08}", i * (KEYSPACE / n.max(1))).into_bytes())
        .collect()
}

#[inline(never)]
fn run_twoprobe(s: &mut Store, keys: &[&[u8]]) -> usize {
    let mut acc = 0usize;
    for &k in keys {
        if let Ok(len) = s.setrange_lfu_twoprobe_bench(k, 0, b"X", 1) {
            acc = acc.wrapping_add(len);
        }
    }
    acc
}

#[inline(never)]
fn run_collapse(s: &mut Store, keys: &[&[u8]]) -> usize {
    let mut acc = 0usize;
    for &k in keys {
        if let Ok(len) = s.setrange(k, 0, b"X", 1) {
            acc = acc.wrapping_add(len);
        }
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
    let owned = setrange_keys(n);
    let keys: Vec<&[u8]> = owned.iter().map(|k| k.as_slice()).collect();

    let mut reps = 1usize;
    loop {
        let e = time(reps, s, run_twoprobe, &keys);
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
        let nn = pair(run_twoprobe, run_twoprobe);
        let sp = pair(run_twoprobe, run_collapse);
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
    println!(
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_setrange", "reps", "NULL med", "null p5..p95", "null cv%", "collapse/2p", "verdict"
    );
    let mut s = build();
    bench("n32", &mut s, 32);
    bench("n256", &mut s, 256);
}
