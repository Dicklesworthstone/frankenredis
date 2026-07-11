//! Same-binary A/B for the set-listpack DUMP encode (frankenredis-lbmk6, cod-a code-first).
//!
//! ORIG (pre-lbmk6): `Store::dump_key` set-listpack branches cloned every member into a
//! `Vec<Vec<u8>>`, staged a `Vec<&[u8]>`, then called `encode_listpack_strings` — one heap
//! alloc per generic member (the `into_owned` clone) and one per intset member (each i64
//! rendered to an owned `Vec` by the set iterator). NEW: `encode_set_listpack_dump` streams
//! members straight into the listpack finalizer — generic members borrow their bytes, intset
//! members stack-render each i64. This isolates the eliminated per-DUMP allocations.
//!
//! Substrate matches the other cc benches (quicklist_encode.rs): ONE binary / ONE invocation,
//! adjacent-pair interleaving (order swapped on odd rounds), `black_box`, reps calibrated per
//! input, median of paired per-round ratios, gated on the candidate median lying outside the
//! null control's p5..p95 spread (`cv` reported, never gated). Both arms emit BYTE-IDENTICAL
//! listpacks (asserted before timing), so the ratio is pure allocation/copy work.

use std::hint::black_box;
use std::time::Instant;

use fr_store::{
    BenchSetListpackDump, bench_set_listpack_dump_ints, bench_set_listpack_dump_strings,
};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// `n` unique short generic string members (mixed lengths, non-integer -> string-encoded).
fn str_set(n: usize) -> BenchSetListpackDump {
    let members: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("member:{i:05}:{}", "abcdefgh".as_bytes()[i % 8] as char).into_bytes())
        .collect();
    bench_set_listpack_dump_strings(&members)
}

/// `n` unique sorted i64 members (intset encoding -> the pre-lbmk6 per-int render allocation).
fn int_set(n: usize) -> BenchSetListpackDump {
    let mut ints: Vec<i64> = (0..n as i64).map(|i| i * 2_654_435 - 7).collect();
    ints.sort_unstable();
    ints.dedup();
    bench_set_listpack_dump_ints(ints)
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
    println!(
        "\n{:<14} {:>7} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );

    let cases: &[(&str, BenchSetListpackDump)] = &[
        ("str_128", str_set(128)),
        ("str_32", str_set(32)),
        ("int_128", int_set(128)),
        ("int_32", int_set(32)),
    ];

    for (label, set) in cases {
        // Correctness gate: both arms produce byte-identical listpack DUMP bytes.
        assert_eq!(
            set.encode_orig(),
            set.encode_new(),
            "{label}: orig/new set-listpack DUMP diverged"
        );

        let orig = |s: &BenchSetListpackDump| s.encode_orig().map_or(0, |v| v.len());
        let cand = |s: &BenchSetListpackDump| s.encode_new().map_or(0, |v| v.len());
        let time = |f: &dyn Fn(&BenchSetListpackDump) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(set)));
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
            let pair = |bf: &dyn Fn(&BenchSetListpackDump) -> usize,
                        cf: &dyn Fn(&BenchSetListpackDump) -> usize| {
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
                continue; // discard warm-up round
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
            "{:<14} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
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
