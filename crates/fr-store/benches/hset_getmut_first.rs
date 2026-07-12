//! Same-binary A/B for the non-LFU `hset_borrowed` entry resolution: `get_mut`-FIRST (ONE keyspace
//! probe for the existing hash) vs the pre-change `internal_entry` (contains_key + get_mut = TWO).
//! Byte-identical (gated by `hset_borrowed_getmut_first_matches_internal_entry_ref`); the delta is
//! the one eliminated hashmap probe on the common existing-key HSET.
//!
//! ORIG = `hset_borrowed_internal_entry_ref` (internal_entry).  CAND = `hset_borrowed` (get_mut-first).
//! One store holds an existing hash; each rep overwrites the SAME field (idempotent — hlen stays
//! fixed), so the store is stable and the measurement is clean (non-destructive), isolating the
//! entry-resolution probe count. LFU is OFF (the path get_mut-first changes).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.05;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn build(fields: usize) -> Store {
    let mut s = Store::new();
    for i in 0..fields {
        s.hset_borrowed(b"h", format!("f{i:05}").as_bytes(), b"v".to_vec(), 1)
            .unwrap();
    }
    s
}

#[inline(never)]
fn run_getmut_first(s: &mut Store) -> bool {
    s.hset_borrowed(b"h", b"f00000", b"w".to_vec(), 2).unwrap()
}
#[inline(never)]
fn run_internal_entry(s: &mut Store) -> bool {
    s.hset_borrowed_internal_entry_ref(b"h", b"f00000", b"w".to_vec(), 2)
        .unwrap()
}

fn time(reps: usize, s: &mut Store, f: fn(&mut Store) -> bool) -> f64 {
    let start = Instant::now();
    let mut acc = false;
    for _ in 0..reps {
        acc ^= f(black_box(s));
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

fn bench(label: &str, fields: usize) {
    let mut s = build(fields);

    let mut reps = 1usize;
    loop {
        let e = time(reps, &mut s, run_internal_entry);
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
        let mut pair = |bf: fn(&mut Store) -> bool, cf: fn(&mut Store) -> bool| {
            if swap {
                let c = time(reps, &mut s, cf);
                time(reps, &mut s, bf) / c
            } else {
                let b = time(reps, &mut s, bf);
                b / time(reps, &mut s, cf)
            }
        };
        let nn = pair(run_internal_entry, run_internal_entry);
        let sp = pair(run_internal_entry, run_getmut_first);
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
        "WIN(getmut-first)"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "{:<12} {:>8} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>18}",
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
        "\n{:<12} {:>8} {:>9} {:>16} {:>8} {:>11} {:>18}",
        "hash_fields", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );
    bench("f1", 1);
    bench("f8", 8);
    bench("f64", 64);
}
