//! Same-binary A/B for SPOP count: the pre-fusion `count`x `spop` loop (one keyspace `get_mut` PER
//! pop) vs the fused `spop_count` (ONE `get_mut` + replayed RNG). Byte-identical (gated by
//! `spop_count_fused_matches_spop_loop`); the delta is the `count - 1` eliminated hashmap lookups.
//!
//! ORIG = `spop_count_loop_ref`.  CAND = `spop_count` (fused, LFU off = the default fast path).
//! The set is rebuilt fresh per rep OUTSIDE the timed region so only the pop call is measured.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.015;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn build(n: usize) -> Store {
    let mut s = Store::new();
    let members: Vec<Vec<u8>> = (0..n).map(|i| format!("member:{i:06}").into_bytes()).collect();
    s.sadd(b"s", &members, 1).unwrap();
    s
}

#[inline(never)]
fn run_loop(s: &mut Store, count: usize) -> usize {
    s.spop_count_loop_ref(b"s", count, 2).unwrap().len()
}
#[inline(never)]
fn run_fused(s: &mut Store, count: usize) -> usize {
    s.spop_count(b"s", count, 2).unwrap().len()
}

/// Time `reps` pop calls, each on a FRESH set built outside the timed span.
fn time(reps: usize, n: usize, count: usize, f: fn(&mut Store, usize) -> usize) -> f64 {
    let mut total = 0.0;
    let mut acc = 0usize;
    for _ in 0..reps {
        let mut s = build(n);
        let start = Instant::now();
        acc = acc.wrapping_add(f(black_box(&mut s), black_box(count)));
        total += start.elapsed().as_secs_f64();
        black_box(&s);
    }
    black_box(acc);
    total
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

fn bench(label: &str, n: usize, count: usize) {
    let mut reps = 1usize;
    loop {
        let e = time(reps, n, count, run_loop);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 16 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let pair = |bf: fn(&mut Store, usize) -> usize, cf: fn(&mut Store, usize) -> usize| {
            if swap {
                let c = time(reps, n, count, cf);
                time(reps, n, count, bf) / c
            } else {
                let b = time(reps, n, count, bf);
                b / time(reps, n, count, cf)
            }
        };
        let nn = pair(run_loop, run_loop);
        let sp = pair(run_loop, run_fused);
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
        "WIN(fused)"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "{:<16} {:>6} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
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
        "\n{:<16} {:>6} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "set/count", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );
    bench("n1000/all", 1000, 1000);
    bench("n1000/c256", 1000, 256);
    bench("n256/all", 256, 256);
    bench("n64/all", 64, 64);
}
