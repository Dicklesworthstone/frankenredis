//! Same-binary A/B for RESP decimal digit-count (frankenredis-e4fu8).
//!
//! `decimal_u64_len` / `decimal_usize_len` / `decimal_i64_len` size RESP integer replies and
//! bulk-string headers — run on the reply hot path (every `:N\r\n` and every `$len\r\n`). ORIG
//! counted digits with a div-by-10 loop (up to ~20 iterations for large ints). e4fu8
//! (`f5e835d45`) replaced it with branchless `ilog10` (a leading-zeros + tiny-table sequence,
//! constant work). Its fr-store sibling `i64_text_len` already measured 5.86x (`03dcd9c51`).
//! Both are mirrored here verbatim; byte-identical count asserted before timing.
//!
//! ORIG = div-loop; CAND = ilog10. verdict WIN => ilog10 is faster.
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of 41 paired ratios, gated on the candidate
//! median outside the null (orig-vs-orig) p5..p95.

use std::hint::black_box;
use std::time::Instant;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// ORIG: the original div-by-10 loop (verbatim from the e4fu8 reference test).
fn div_loop_len(mut n: u64) -> usize {
    let mut len = 1;
    while n >= 10 {
        n /= 10;
        len += 1;
    }
    len
}
/// CAND: branchless ilog10 (verbatim from `decimal_u64_len`).
fn ilog10_len(n: u64) -> usize {
    if n == 0 { 1 } else { n.ilog10() as usize + 1 }
}

/// Batch of `n` u64 in the digit-width band around `10^(digits-1)` (div-loop cost scales with
/// digit count; ilog10 is constant — so the win grows with width).
fn band(n: usize, digits: u32) -> Vec<u64> {
    let base = 10u64.saturating_pow(digits.saturating_sub(1)).max(1);
    (0..n as u64).map(|i| base.wrapping_add(i.wrapping_mul(7))).collect()
}
/// Realistic RESP integer/length mix: mostly small (1-4 digit lengths/counts) with a tail of
/// large values (INCR counters, big cardinalities).
fn resp_mix(n: usize) -> Vec<u64> {
    (0..n as u64)
        .map(|i| match i % 8 {
            0..=4 => i % 1000,            // small: 1-3 digits (lengths, small ints)
            5 => 100_000 + i,             // 6 digits
            6 => 10_000_000_000 + i,      // 11 digits
            _ => u64::MAX - i,            // 20 digits
        })
        .collect()
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
    // Correctness gate: identical count across digit boundaries + extremes.
    let mut probes: Vec<u64> = vec![0, 1, 9, 10, 11, 99, 100, 101, u64::MAX, u64::MAX - 1];
    let mut p: u64 = 1;
    loop {
        probes.push(p.saturating_sub(1));
        probes.push(p);
        probes.push(p.saturating_add(1));
        match p.checked_mul(10) {
            Some(next) => p = next,
            None => break,
        }
    }
    for &n in &probes {
        assert_eq!(div_loop_len(n), ilog10_len(n), "digit count for {n}");
    }

    println!(
        "\n{:<12} {:>7} {:>9} {:>16} {:>8} {:>13} {:>14}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "ilog/divloop", "verdict"
    );

    let cases: &[(&str, Vec<u64>)] = &[
        ("len_1_4_512", band(512, 3)),
        ("d11_512", band(512, 11)),
        ("d20_512", band(512, 20)),
        ("resp_mix_512", resp_mix(512)),
    ];

    for (label, vals) in cases {
        let orig = |vs: &[u64]| vs.iter().map(|&n| div_loop_len(n)).sum::<usize>();
        let cand = |vs: &[u64]| vs.iter().map(|&n| ilog10_len(n)).sum::<usize>();
        let time = |f: &dyn Fn(&[u64]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(vals)));
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
            let pair = |bf: &dyn Fn(&[u64]) -> usize, cf: &dyn Fn(&[u64]) -> usize| {
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
            "WIN(ilog10)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<12} {:>7} {:>9.4} {:>16} {:>8.2} {:>12.3}x {:>14}",
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
