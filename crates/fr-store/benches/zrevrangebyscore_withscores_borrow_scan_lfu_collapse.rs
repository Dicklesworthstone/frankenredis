//! Same-binary A/B for the LFU ZREVRANGEBYSCORE WITHSCORES keyspace-probe collapse on the ZERO-COPY
//! production path (`zrevrangebyscore_withscores_borrow_scan`, the fr-runtime borrow-scan encoder). The
//! non-LFU path already single-probes via `lookup_live_for_read_mut`; this folds the LFU path's
//! `record_keyspace_lookup` + `contains_key` rand-gate + `get_mut` into ONE `get_mut` (expiry peek +
//! inline hit/miss + `rand_sample` on a disjoint `&mut self.rng_seed` field split). 3 probes → 1.
//! Byte/RNG/stat-identical (`zrevrangebyscore_withscores_borrow_scan_lfu_collapsed_matches_threeprobe`).
//!
//! Non-mutating → repeatable. Each timed op loops a full-range ZRANGEBYSCORE (-inf, +inf) over small
//! zsets, summing member byte lengths through the sink. CAND = production
//! `zrevrangebyscore_withscores_borrow_scan` (`::<true>`), ORIG = the `_lfu_threeprobe_bench` baseline.

use std::hint::black_box;
use std::time::Instant;

use fr_store::{MaxmemoryPolicy, ScoreBound, ZRangeWithScoresScanEvent, Store};

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
        let k = format!("k{i:08}").into_bytes();
        s.zadd(
            &k,
            &[(1.0, b"a".to_vec()), (2.0, b"b".to_vec()), (3.0, b"c".to_vec())],
            1,
        )
        .unwrap();
    }
    s
}

fn scan_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("k{:08}", i * (KEYSPACE / n.max(1))).into_bytes())
        .collect()
}

fn full_range() -> (ScoreBound, ScoreBound) {
    (
        ScoreBound::Inclusive(f64::NEG_INFINITY),
        ScoreBound::Inclusive(f64::INFINITY),
    )
}

#[inline(never)]
fn run_threeprobe(s: &mut Store, keys: &[&[u8]]) -> usize {
    let (min, max) = full_range();
    let mut acc = 0usize;
    for &k in keys {
        let mut local = 0usize;
        let _ = s.zrevrangebyscore_withscores_borrow_scan_lfu_threeprobe_bench(k, min, max, 1, |ev| {
            if let ZRangeWithScoresScanEvent::Pair(m, _sc) = ev {
                local = local.wrapping_add(m.len());
            }
        });
        acc = acc.wrapping_add(local);
    }
    acc
}

#[inline(never)]
fn run_collapse(s: &mut Store, keys: &[&[u8]]) -> usize {
    let (min, max) = full_range();
    let mut acc = 0usize;
    for &k in keys {
        let mut local = 0usize;
        let _ = s.zrevrangebyscore_withscores_borrow_scan(k, min, max, 1, |ev| {
            if let ZRangeWithScoresScanEvent::Pair(m, _sc) = ev {
                local = local.wrapping_add(m.len());
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
    println!(
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_zrevbyscws", "reps", "NULL med", "null p5..p95", "null cv%", "collapse/3p", "verdict"
    );
    let mut s = build();
    bench("n32", &mut s, 32);
    bench("n256", &mut s, 256);
}
