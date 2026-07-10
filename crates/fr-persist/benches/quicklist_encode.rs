//! Same-binary A/B for the quicklist2 list RDB encode: the two-walk original (a
//! `quicklist2_node_count` pre-walk that recomputes `listpack_entry_encoded_len` per item, then a
//! pack loop that recomputes it again) vs the memoized version (compute each item's length ONCE,
//! feed both the count and the pack loop), null-gated on the median.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated per input, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated).
//!
//! Both arms emit BYTE-IDENTICAL RDB (asserted before timing), so the LZF work per node is identical
//! and the ratio isolates the duplicated length computation. Workloads: integer items (where
//! `parse_listpack_integer` succeeds and the length cost is comparable to LZF per node — the win),
//! short strings, and long (>=21 byte) strings (where the parse rejects immediately — a wash, and a
//! regression guard).

use std::hint::black_box;
use std::time::Instant;

use fr_persist::{
    CompactRdbThresholds, encode_compact_list_quicklist2_new, encode_compact_list_quicklist2_orig,
};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// A multi-node list of small integers (each item parses as an integer).
fn int_list(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| ((i as i64) * 2_654_435 - 7).to_string().into_bytes()).collect()
}
/// A multi-node list of short non-integer strings (parse fails after a few bytes).
fn short_str_list(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("e{i}x").into_bytes()).collect()
}
/// A multi-node list of long (>=21 byte) strings (parse rejects on `len >= 21`).
fn long_str_list(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("member:{i:08}:payload:{i:08}").into_bytes()).collect()
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
    let th = CompactRdbThresholds::default();
    println!(
        "\n{:<16} {:>7} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );

    let cases: &[(&str, Vec<Vec<u8>>)] = &[
        ("int_9000", int_list(9000)),
        ("short_str_9000", short_str_list(9000)),
        ("long_str_4000", long_str_list(4000)),
    ];

    for (label, items) in cases {
        // Correctness gate: byte-identical RDB from both arms.
        assert_eq!(
            encode_compact_list_quicklist2_orig(items, &th),
            encode_compact_list_quicklist2_new(items, &th),
            "{label}: orig/new RDB diverged"
        );

        let orig = |it: &[Vec<u8>]| encode_compact_list_quicklist2_orig(it, &th).map_or(0, |v| v.len());
        let cand = |it: &[Vec<u8>]| encode_compact_list_quicklist2_new(it, &th).map_or(0, |v| v.len());
        let time = |f: &dyn Fn(&[Vec<u8>]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(items)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&orig, reps);
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
            let pair = |bf: &dyn Fn(&[Vec<u8>]) -> usize, cf: &dyn Fn(&[Vec<u8>]) -> usize| {
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
            "WIN"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<16} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
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
