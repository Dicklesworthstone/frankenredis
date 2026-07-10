//! Same-binary A/B for `parse_listpack_integer` (the DUMP/RDB-save int-encode gate): the pre-fuse
//! two-pass version (`_orig`, canonical-scan + accumulate-scan) vs the single-pass fused version
//! (`_new`), null-gated on the median.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated per input, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated).
//!
//! Inputs model the entries `encode_listpack_entry` int-tests on DUMP/RDB-save: canonical decimal
//! byte-strings across magnitudes (the fusion's target — these pass the canonical check, so the old
//! path scanned the digits twice), plus a realistic slice of non-integer members (both paths reject
//! on the first non-digit, so no benefit there). Both arms return the identical `Option<i64>` sum
//! (asserted before timing).

use std::hint::black_box;
use std::time::Instant;

use fr_persist::{parse_listpack_integer_new, parse_listpack_integer_orig};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn all_ints(n: usize) -> Vec<Vec<u8>> {
    // Spread magnitudes 1..19 digits, mixed sign — every entry is a canonical int.
    (0..n)
        .map(|i| {
            let mag = (i % 19) as u32;
            let base = 10i128.saturating_pow(mag);
            let v = (base + i as i128 * 7) as i64;
            if i % 3 == 0 { -v } else { v }.to_string().into_bytes()
        })
        .collect()
}

fn mixed(n: usize) -> Vec<Vec<u8>> {
    // ~half canonical ints, ~half string members (reject fast in both arms).
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                ((i as i64) * 2_654_435 - 7).to_string().into_bytes()
            } else {
                format!("member:{i:08}:tag").into_bytes()
            }
        })
        .collect()
}

fn sum_orig(items: &[Vec<u8>]) -> i64 {
    items.iter().filter_map(|e| parse_listpack_integer_orig(e)).fold(0i64, i64::wrapping_add)
}
fn sum_new(items: &[Vec<u8>]) -> i64 {
    items.iter().filter_map(|e| parse_listpack_integer_new(e)).fold(0i64, i64::wrapping_add)
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

    let cases: &[(&str, Vec<Vec<u8>>)] =
        &[("all_ints_2048", all_ints(2048)), ("mixed_2048", mixed(2048))];

    for (label, items) in cases {
        // Correctness gate.
        assert_eq!(sum_orig(items), sum_new(items), "{label}: orig/new diverged");

        let orig = |it: &[Vec<u8>]| sum_orig(it);
        let cand = |it: &[Vec<u8>]| sum_new(it);
        let time = |f: &dyn Fn(&[Vec<u8>]) -> i64, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0i64;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(items)));
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
            let pair = |bf: &dyn Fn(&[Vec<u8>]) -> i64, cf: &dyn Fn(&[Vec<u8>]) -> i64| {
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
