//! Same-binary A/B for the post-insert hash encoding refresh on the non-LFU `hset_borrowed` path:
//! O(1) INCREMENTAL (`INCR=true`, check the entry count + the JUST-inserted field/value lengths)
//! vs the O(n) RE-SCAN (`INCR=false`, `refresh_hash_encoding_flag` walks EVERY field/value each
//! HSET). A listpack hash (flag unset) has every existing field/value <= max-listpack-value by
//! invariant (every hash write path refreshes; an oversized field would already have set the flag),
//! so only the count and the new field/value can drive the one-way listpack->hashtable promotion —
//! rescanning the guaranteed-small existing fields is pure O(n)-per-HSET waste (O(n^2) building a
//! listpack hash). The incremental decision is byte-identical to the rescan AND matches redis's own
//! `hashTypeTryConversion`, which likewise inspects only the incoming args.
//!
//! ORIG = `hset_borrowed_rescan` (INCR=false).  CAND = `hset_borrowed` (INCR=true).
//! One store holds an existing listpack hash of `fields` entries (all under 512 = still listpack);
//! each rep overwrites the SAME field (idempotent — hlen fixed), so the store is stable and the
//! measurement isolates the refresh cost: ORIG walks `fields` entries, CAND checks three lengths.
//! LFU is OFF (the non-LFU common path the refresh lives on).

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
fn run_incremental(s: &mut Store) -> bool {
    s.hset_borrowed(b"h", b"f00000", b"w".to_vec(), 2).unwrap()
}
#[inline(never)]
fn run_rescan(s: &mut Store) -> bool {
    s.hset_borrowed_rescan(b"h", b"f00000", b"w".to_vec(), 2)
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
        let e = time(reps, &mut s, run_rescan);
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
        let nn = pair(run_rescan, run_rescan);
        let sp = pair(run_rescan, run_incremental);
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
        "WIN(incremental)"
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
    bench("f8", 8);
    bench("f64", 64);
    bench("f256", 256);
    bench("f511", 511);
}
