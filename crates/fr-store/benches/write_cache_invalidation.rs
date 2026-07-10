//! Same-binary A/B for the per-write side-cache invalidation that runs on every scalar write
//! (SET/INCR insert, DEL, in-place INCR): the pre-guard original (unconditional `remove` on the
//! HLL-register / DUMP-payload / MEMORY-estimate maps, hashing the key against each even when empty)
//! vs the `is_empty()`-guarded version. Null-gated on the median.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated once, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated).
//!
//! The three caches are EMPTY for the vast majority of keys (only PFADD / DUMP / MEMORY USAGE on a
//! key populates them), so this measures the dominant path: the guard skips three foldhashes of the
//! key where the removal was a no-op anyway. Both arms leave the (empty) caches unchanged, so the
//! store state is stable across all reps; the helper runs per write, so its delta is a per-write
//! saving on SET/INCR/DEL. Byte-identical (asserted by the `invalidate_write_side_caches_matches_orig`
//! unit test).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"counter:key:00000042";

fn orig(s: &mut Store) {
    s.invalidate_write_side_caches_orig(black_box(KEY));
}
fn newp(s: &mut Store) {
    s.invalidate_write_side_caches_new(black_box(KEY));
}

fn timed(f: fn(&mut Store), s: &mut Store, reps: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..reps {
        f(s);
    }
    black_box(&*s);
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

fn main() {
    let mut store_o = Store::new();
    let mut store_n = Store::new();

    let mut reps = 1usize;
    loop {
        let e = timed(orig, &mut store_o, reps);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 22 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let nn = if swap {
            let c = timed(orig, &mut store_o, reps);
            timed(orig, &mut store_o, reps) / c
        } else {
            let b = timed(orig, &mut store_o, reps);
            b / timed(orig, &mut store_o, reps)
        };
        let sp = if swap {
            let c = timed(newp, &mut store_n, reps);
            timed(orig, &mut store_o, reps) / c
        } else {
            let b = timed(orig, &mut store_o, reps);
            b / timed(newp, &mut store_n, reps)
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
        "\n{:<24} {:>8} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    println!(
        "{:<24} {:>8} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
        "invalidate_side_caches",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
