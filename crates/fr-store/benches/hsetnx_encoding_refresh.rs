//! Same-binary A/B for the post-insert hash encoding refresh on HSETNX. HSETNX only refreshes on a
//! GENUINE insert (a no-op on an existing field never touches the encoding), so unlike HSET/HINCRBY
//! there is no idempotent-overwrite isolate — the real cost is BUILDING a hash one absent field at a
//! time: the O(n) `refresh_hash_encoding_flag` re-scan (`INCR=false`) walks every field on every
//! insert, making an F-field build O(n^2); the O(1) incremental refresh (`INCR=true`, count + the
//! new field/value lengths) makes it O(n). Byte-identical decision (gated by
//! `hsetnx_incremental_refresh_matches_rescan`).
//!
//! ORIG = `hsetnx_rescan` (INCR=false).  CAND = `hsetnx` (INCR=true).
//! The timed unit is a full fresh F-field build (all under 512 = stays listpack, so the flag never
//! sets and the refresh runs on every insert). Field names are pre-materialized once so the loop
//! measures HSETNX (probe + insert + refresh), not `format!`. LFU is OFF.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.05;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn fields_of(fields: usize) -> Vec<Vec<u8>> {
    (0..fields).map(|i| format!("f{i:04}").into_bytes()).collect()
}

#[inline(never)]
fn build_incremental(names: &[Vec<u8>]) -> u64 {
    let mut s = Store::new();
    for n in names {
        s.hsetnx(b"h", n.clone(), b"v".to_vec(), 1).unwrap();
    }
    black_box(&s);
    names.len() as u64
}
#[inline(never)]
fn build_rescan(names: &[Vec<u8>]) -> u64 {
    let mut s = Store::new();
    for n in names {
        s.hsetnx_rescan(b"h", n.clone(), b"v".to_vec(), 1).unwrap();
    }
    black_box(&s);
    names.len() as u64
}

fn time(reps: usize, names: &[Vec<u8>], f: fn(&[Vec<u8>]) -> u64) -> f64 {
    let start = Instant::now();
    let mut acc = 0u64;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(names)));
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
    let names = fields_of(fields);

    let mut reps = 1usize;
    loop {
        let e = time(reps, &names, build_rescan);
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
        let mut pair = |bf: fn(&[Vec<u8>]) -> u64, cf: fn(&[Vec<u8>]) -> u64| {
            if swap {
                let c = time(reps, &names, cf);
                time(reps, &names, bf) / c
            } else {
                let b = time(reps, &names, bf);
                b / time(reps, &names, cf)
            }
        };
        let nn = pair(build_rescan, build_rescan);
        let sp = pair(build_rescan, build_incremental);
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
        "build_fields", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );
    bench("f8", 8);
    bench("f64", 64);
    bench("f256", 256);
    bench("f511", 511);
}
