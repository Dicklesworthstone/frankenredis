//! Same-binary A/B for listpack RDB-decode string materialization (frankenredis-knzdi).
//!
//! `decode_rdb_prefix` decodes RDB_TYPE_{SET,HASH,ZSET}_LISTPACK by calling `decode_listpack`
//! (which copies each string entry's bytes OUT of the blob into an owned `Vec<u8>`) and then
//! materializing each entry. ORIG called `ListpackEntry::to_bytes(&self)` — for a String that
//! CLONES the just-decoded `Vec<u8>` and drops the original (2 string-copies total). knzdi
//! (`071cdc75b`) switched to `ListpackEntry::into_bytes(self)`, which MOVES the decoded payload
//! out (1 string-copy total). This A/B measures the production-faithful decode+materialize
//! pattern so the ratio reflects the real win (or shows the clone is absorbed).
//!
//! ORIG = to_bytes (clone); CAND = into_bytes (move). verdict WIN => knzdi is faster.
//! Integer entries render decimals identically in both arms (a regression guard / wash).
//!
//! Substrate = the cc bench roster (quicklist_encode.rs): ONE binary, adjacent-pair interleave
//! (swap on odd rounds), black_box, reps calibrated per input, median of 41 paired ratios,
//! gated on the candidate median outside the null (orig-vs-orig) p5..p95. Both arms produce
//! BYTE-IDENTICAL materialized entries (asserted before timing).

use std::hint::black_box;
use std::time::Instant;

use fr_persist::encode_listpack_strings_blob;
use fr_persist::listpack::{ListpackEntry, decode_listpack};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// A listpack blob of `n` short non-integer string entries (~`w` bytes each).
fn str_blob(n: usize, w: usize) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut s = format!("member:{i:06}:").into_bytes();
            s.resize(w.max(s.len()), b'x');
            s
        })
        .collect();
    let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    encode_listpack_strings_blob(&refs).expect("encode str listpack")
}

/// A listpack blob of `n` integer entries (both arms render decimals — a wash / guard).
fn int_blob(n: usize) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = (0..n).map(|i| ((i as i64) * 733 - 11).to_string().into_bytes()).collect();
    let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    encode_listpack_strings_blob(&refs).expect("encode int listpack")
}

// ORIG: decode + to_bytes (clone each string). Production-faithful (decode is common to both arms).
fn materialize_to_bytes(blob: &[u8]) -> usize {
    decode_listpack(blob)
        .expect("decode")
        .iter()
        .map(|e| e.to_bytes().len())
        .sum()
}
// CAND (knzdi): decode + into_bytes (move each string).
fn materialize_into_bytes(blob: &[u8]) -> usize {
    decode_listpack(blob)
        .expect("decode")
        .into_iter()
        .map(|e| ListpackEntry::into_bytes(e).len())
        .sum()
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
    // Correctness gate: to_bytes and into_bytes materialize byte-identical entries.
    for (label, blob) in [("str", str_blob(64, 16)), ("int", int_blob(64))] {
        let a: Vec<Vec<u8>> = decode_listpack(&blob).unwrap().iter().map(|e| e.to_bytes()).collect();
        let b: Vec<Vec<u8>> =
            decode_listpack(&blob).unwrap().into_iter().map(ListpackEntry::into_bytes).collect();
        assert_eq!(a, b, "{label}: to_bytes/into_bytes diverged");
    }

    println!(
        "\n{:<14} {:>7} {:>9} {:>16} {:>8} {:>11} {:>14}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "move/clone", "verdict"
    );

    let cases: &[(&str, Vec<u8>)] = &[
        ("str16_128", str_blob(128, 16)),
        ("str40_128", str_blob(128, 40)),
        ("str16_32", str_blob(32, 16)),
        ("int_128", int_blob(128)),
    ];

    for (label, blob) in cases {
        let orig = |b: &[u8]| materialize_to_bytes(b);
        let cand = |b: &[u8]| materialize_into_bytes(b);
        let time = |f: &dyn Fn(&[u8]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(blob)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&orig, reps);
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
            let pair = |bf: &dyn Fn(&[u8]) -> usize, cf: &dyn Fn(&[u8]) -> usize| {
                if swap {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let nn = pair(&orig, &orig);
            let sp = pair(&orig, &cand);
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
            "WIN(move)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<14} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>14}",
            label,
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            speedup,
            verdict
        );
    }
}
