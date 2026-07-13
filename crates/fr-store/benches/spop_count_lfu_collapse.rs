//! Same-binary A/B for the LFU `spop_count` keyspace-probe collapse. Under an allkeys-lfu policy
//! the prior `spop_count` delegated to `spop_count_loop_ref` — `count`× `self.spop(key)`, each doing
//! `drop_if_expired` + `contains_key` (for the lfu_rand gate) + `get_mut` per popped member (O(count)
//! keyspace probes). The fused path holds ONE `get_mut` for the whole batch and draws the per-pop
//! `rand_val`/`lfu_rand` on a disjoint `&mut self.rng_seed` field split, bumping LFU per pop — so a
//! drain of an N-member set costs O(N/count) probes instead of O(N). Byte/RNG-identical (gated by
//! `spop_count_fused_matches_spop_loop`, LFU + non-LFU).
//!
//! SPOP is destructive, so this is a DRAIN A/B: the target set is refilled UNTIMED before each timed
//! drain (both arms drain an identical N-member set out of an identical large keyspace). A big
//! keyspace makes each keyspace probe a realistic `HashMap` access (cache behaviour). ORIG = loop_ref
//! drain, CAND = fused drain; speedup = loop_time / fused_time (>1 ⇒ fused faster).

use std::hint::black_box;
use std::time::Instant;

use fr_store::{MaxmemoryPolicy, Store};

const ROUNDS: usize = 41;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

const KEYSPACE: usize = 20_000;
const SET_N: usize = 10_000;

fn build_base() -> Store {
    let mut s = Store::new();
    s.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    s.lfu_decay_time = 0;
    for i in 0..KEYSPACE {
        s.set(format!("k{i:08}").into_bytes(), b"v".to_vec(), None, 1);
    }
    s
}

fn refill(s: &mut Store) {
    let members: Vec<Vec<u8>> = (0..SET_N).map(|i| format!("m{i:07}").into_bytes()).collect();
    s.sadd(b"target", &members, 1).expect("sadd");
}

#[inline(never)]
fn drain_fused(s: &mut Store, count: usize) -> usize {
    let mut popped = 0usize;
    loop {
        let r = s.spop_count(b"target", count, 1).expect("ok");
        if r.is_empty() {
            break;
        }
        popped += r.len();
    }
    popped
}
#[inline(never)]
fn drain_loop(s: &mut Store, count: usize) -> usize {
    let mut popped = 0usize;
    loop {
        let r = s.spop_count_loop_ref(b"target", count, 1).expect("ok");
        if r.is_empty() {
            break;
        }
        popped += r.len();
    }
    popped
}

fn timed(s: &mut Store, f: fn(&mut Store, usize) -> usize, count: usize) -> f64 {
    refill(s);
    let t = Instant::now();
    black_box(f(black_box(s), black_box(count)));
    t.elapsed().as_secs_f64()
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

fn bench(label: &str, s: &mut Store, count: usize) {
    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        // null: fused vs fused
        let n1 = timed(s, drain_fused, count);
        let n2 = timed(s, drain_fused, count);
        let nn = n1 / n2;
        // speed: loop vs fused (interleave order swaps to cancel drift)
        let sp = if swap {
            let fu = timed(s, drain_fused, count);
            let lp = timed(s, drain_loop, count);
            lp / fu
        } else {
            let lp = timed(s, drain_loop, count);
            let fu = timed(s, drain_fused, count);
            lp / fu
        };
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
        "{:<14} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
        label,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}

fn main() {
    println!(
        "\n{:<14} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "count", "NULL med", "null p5..p95", "null cv%", "loop/fused", "verdict"
    );
    let mut s = build_base();
    bench("count10", &mut s, 10);
    bench("count100", &mut s, 100);
    bench("count1000", &mut s, 1000);
}
