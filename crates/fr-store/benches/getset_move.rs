//! Same-binary A/B for GETSET's old-value handling on a large string value: cloning the old
//! value out (`getset_orig`, MOVE=false) vs moving it out (`getset`, MOVE=true — the entry is
//! overwritten immediately, so the old `Heap` `Vec` can be moved rather than copied). Null-gated
//! on the median.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated once, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated).
//!
//! Overwriting the key with the SAME (large) value keeps the store a single stable entry across all
//! reps. Both arms still clone the NEW value from the borrowed param and pay one owned-key arg alloc
//! (diluting the ratio); the isolated difference is the OLD-value clone vs move. Byte-identical
//! (asserted by the `getset_move_matches_clone_orig` unit test).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"gs:key";
// The move-vs-clone gain is proportional to the old value's size (ratio → 2x as it grows,
// → 1x for a <=15-byte inline value). A large value makes the win decisive against the
// shared-worker noise floor; the isolated saving is one memcpy of the old value.
const VALLEN: usize = 65_536;

fn val() -> Vec<u8> {
    (0..VALLEN).map(|i| b'a' + (i % 26) as u8).collect()
}

fn build_store() -> Store {
    let mut s = Store::new();
    s.set(KEY.to_vec(), val(), None, 2_000);
    s
}

fn getset_orig(s: &mut Store, v: &[u8]) {
    let _ = s.getset_orig(black_box(KEY).to_vec(), black_box(v), 2_000);
}
fn getset_new(s: &mut Store, v: &[u8]) {
    let _ = s.getset(black_box(KEY).to_vec(), black_box(v), 2_000);
}

fn timed(f: fn(&mut Store, &[u8]), s: &mut Store, v: &[u8], reps: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..reps {
        f(s, v);
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
    let v = val();
    let mut store_o = build_store();
    let mut store_n = build_store();

    let mut reps = 1usize;
    loop {
        let e = timed(getset_orig, &mut store_o, &v, reps);
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
            let c = timed(getset_orig, &mut store_o, &v, reps);
            timed(getset_orig, &mut store_o, &v, reps) / c
        } else {
            let b = timed(getset_orig, &mut store_o, &v, reps);
            b / timed(getset_orig, &mut store_o, &v, reps)
        };
        let sp = if swap {
            let c = timed(getset_new, &mut store_n, &v, reps);
            timed(getset_orig, &mut store_o, &v, reps) / c
        } else {
            let b = timed(getset_orig, &mut store_o, &v, reps);
            b / timed(getset_new, &mut store_n, &v, reps)
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
        "getset_4096_move",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
