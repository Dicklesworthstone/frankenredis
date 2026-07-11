//! (frankenredis-bfincrfast) Same-binary A/B for BITFIELD INCRBY's keyspace resolution. The generic
//! command path read the current value with `bitfield_get_no_stat` (one keyspace lookup), computed
//! the clamped new value, then wrote it with `bitfield_set` (a SECOND lookup) — two `get`/`get_mut`
//! where upstream `bitfieldGeneric` resolves the key ONCE. `bitfield_incrby` folds the read into the
//! write's `get_mut`. This bench isolates exactly that: the old two-lookup pair
//! (`bitfield_get_no_stat` + `bitfield_set`) vs the fused `bitfield_incrby`, both applying the same
//! wrapping +1 to a `u8` field on a pre-existing string key (so the store stays a single stable
//! entry). The isolated difference is the eliminated second keyspace lookup; byte-identical apply is
//! proven by the `bitfield_incrby_matches_getnostat_then_set_or_reserve` unit test.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated once, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"bf:key";

fn build_store() -> Store {
    let mut s = Store::new();
    s.set(KEY.to_vec(), vec![0u8; 8], None, 2_000);
    s
}

fn incr_two_lookup(s: &mut Store) {
    let c = s
        .bitfield_get_no_stat(black_box(KEY), 0, 8, false, 2_000)
        .unwrap();
    let _ = s.bitfield_set(black_box(KEY), 0, 8, c.wrapping_add(1), 2_000);
}
fn incr_fused(s: &mut Store) {
    let _ = s.bitfield_incrby(black_box(KEY), 0, 8, false, 2_000, |c| {
        Some(c.wrapping_add(1))
    });
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
    let mut store_o = build_store();
    let mut store_n = build_store();

    let mut reps = 1usize;
    loop {
        let e = timed(incr_two_lookup, &mut store_o, reps);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 18 {
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
            let c = timed(incr_two_lookup, &mut store_o, reps);
            timed(incr_two_lookup, &mut store_o, reps) / c
        } else {
            let b = timed(incr_two_lookup, &mut store_o, reps);
            b / timed(incr_two_lookup, &mut store_o, reps)
        };
        let sp = if swap {
            let c = timed(incr_fused, &mut store_n, reps);
            timed(incr_two_lookup, &mut store_o, reps) / c
        } else {
            let b = timed(incr_two_lookup, &mut store_o, reps);
            b / timed(incr_fused, &mut store_n, reps)
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
        "\n{:<20} {:>7} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    println!(
        "{:<20} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
        "bitfield_incrby",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
