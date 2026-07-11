//! Same-binary A/B for i64->decimal-bytes rendering (frankenredis-tgr69/ef928/087qq).
//!
//! The integer materialization paths (packed-int decode, RDB/ziplist integer restore, listpack
//! int decode) render an i64 to its canonical decimal bytes. ORIG used `i64::to_string()
//! .into_bytes()` — the `core::fmt` Display machinery + a String alloc. The itoa2 conversion
//! (shared primitive `decimal_i64_scratch` -> `fr_protocol::write_u64_digits`, mirrored here)
//! writes digits directly into a stack buffer, then one required result `Vec`. BOTH do exactly
//! one heap alloc (the result), so this isolates the COMPUTE win (direct digit writing vs the
//! fmt machinery) — NOT an alloc elision (the render always needs its result Vec).
//!
//! ORIG = to_string; CAND = write_u64_digits scratch. verdict WIN => itoa2 render is faster.
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of 41 paired ratios, gated on the candidate
//! median outside the null (orig-vs-orig) p5..p95. Both arms produce BYTE-IDENTICAL bytes.

use std::hint::black_box;
use std::time::Instant;

use fr_protocol::write_u64_digits;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// CAND: mirror of fr-persist `decimal_i64_scratch` + `decimal_i64_bytes` (the shipped itoa2 path).
fn itoa2_bytes(value: i64) -> Vec<u8> {
    let mut scratch = [0u8; 20];
    let mut start = write_u64_digits(&mut scratch, 20, value.unsigned_abs());
    if value < 0 {
        start -= 1;
        scratch[start] = b'-';
    }
    scratch[start..].to_vec()
}
/// ORIG: the pre-itoa2 fmt+alloc path.
fn to_string_bytes(value: i64) -> Vec<u8> {
    value.to_string().into_bytes()
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
    // Correctness gate: byte-identical rendering across sign edges, zero, i64 extremes, widths.
    for v in [
        0i64, 1, -1, 9, -9, 10, -10, 99, -99, 100, -100, 12345, -12345, i64::MIN, i64::MAX,
        i64::MIN + 1, i64::MAX - 1, 1_000_000_000_000, -1_000_000_000_000,
    ] {
        assert_eq!(itoa2_bytes(v), to_string_bytes(v), "render diverged on {v}");
    }

    // Batches of i64 spanning the digit-width distribution seen on int-heavy collections.
    fn batch(n: usize, digits: u32) -> Vec<i64> {
        let base = 10i64.pow(digits.saturating_sub(1)).max(1);
        (0..n as i64)
            .map(|i| {
                let v = base + i * 7;
                if i % 4 == 0 { -v } else { v }
            })
            .collect()
    }
    let cases: &[(&str, Vec<i64>)] = &[
        ("d1_256", batch(256, 1)),
        ("d6_256", batch(256, 6)),
        ("d18_256", batch(256, 18)),
    ];

    println!(
        "\n{:<12} {:>7} {:>9} {:>16} {:>8} {:>13} {:>14}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "itoa2/tostr", "verdict"
    );

    for (label, vals) in cases {
        let orig = |vs: &[i64]| vs.iter().map(|&v| to_string_bytes(v).len()).sum::<usize>();
        let cand = |vs: &[i64]| vs.iter().map(|&v| itoa2_bytes(v).len()).sum::<usize>();
        let time = |f: &dyn Fn(&[i64]) -> usize, reps: usize| -> f64 {
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
            let pair = |bf: &dyn Fn(&[i64]) -> usize, cf: &dyn Fn(&[i64]) -> usize| {
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
            "WIN(itoa2)"
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
