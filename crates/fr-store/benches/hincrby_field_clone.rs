//! (frankenredis-hincrfast) Same-binary A/B for HINCRBY/HINCRBYFLOAT field-key handling on an
//! EXISTING hash field (the counter steady state): the old `insert(field.to_vec(), value)` —
//! which allocates an owned field key and extracts the old value — vs `insert_borrowed(field,
//! value)`, which overwrites the value slot in place with no field alloc (and, on the hashtable
//! path, collapses the contains_key+insert double probe into one get_mut). Null-gated on the median.
//!
//! Redis's `hashTypeSet` on an existing field keeps the field sds and replaces only the value sds,
//! so fr's owned-field-key alloc is pure overhead redis never pays. Measured for BOTH encodings:
//! a small listpack hash (the common small-counter case) and a large hashtable hash.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated once, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated). Both arms re-set the SAME field to the SAME value, so the hash stays a
//! single stable entry of constant size across all reps; both pay one owned-value alloc (the real
//! HINCRBY formats `next.to_string()`), so the isolated difference is the field-key handling.
//! Byte-identical map result is guaranteed by the `insert_borrowed` contract (see packed_set.rs).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"h:key";
const FIELD: &[u8] = b"counter:0007";
// Small integer-string value, as a real HINCRBY counter would hold.
fn val() -> Vec<u8> {
    b"1234567".to_vec()
}

/// Small listpack-encoded hash (the common small-counter case): stays under the default
/// hash-max-listpack-entries (128) / -value (64) thresholds.
fn build_listpack() -> Store {
    let mut s = Store::new();
    for i in 0..16u32 {
        s.hset(KEY, format!("counter:{i:04}").into_bytes(), val(), 2_000)
            .expect("hset");
    }
    s
}

/// Large hashtable-encoded hash: >128 fields forces promotion out of the listpack.
fn build_hashtable() -> Store {
    let mut s = Store::new();
    for i in 0..300u32 {
        s.hset(KEY, format!("counter:{i:04}").into_bytes(), val(), 2_000)
            .expect("hset");
    }
    s
}

fn set_owned(s: &mut Store, v: &[u8]) {
    s.bench_hash_field_set_owned(black_box(KEY), black_box(FIELD), black_box(v).to_vec());
}
fn set_borrowed(s: &mut Store, v: &[u8]) {
    s.bench_hash_field_set_borrowed(black_box(KEY), black_box(FIELD), black_box(v).to_vec());
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

fn run_ab(label: &str, mut store_o: Store, mut store_n: Store) {
    let v = val();

    let mut reps = 1usize;
    loop {
        let e = timed(set_owned, &mut store_o, &v, reps);
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
            let c = timed(set_owned, &mut store_o, &v, reps);
            timed(set_owned, &mut store_o, &v, reps) / c
        } else {
            let b = timed(set_owned, &mut store_o, &v, reps);
            b / timed(set_owned, &mut store_o, &v, reps)
        };
        let sp = if swap {
            let c = timed(set_borrowed, &mut store_n, &v, reps);
            timed(set_owned, &mut store_o, &v, reps) / c
        } else {
            let b = timed(set_owned, &mut store_o, &v, reps);
            b / timed(set_borrowed, &mut store_n, &v, reps)
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
        "{:<20} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
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
        "\n{:<20} {:>7} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    run_ab("hincr_listpack", build_listpack(), build_listpack());
    run_ab("hincr_hashtable", build_hashtable(), build_hashtable());
}
