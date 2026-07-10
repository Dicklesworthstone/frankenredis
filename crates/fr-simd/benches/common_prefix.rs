//! Same-binary A/B for the LZF match-extension kernel `common_prefix_len`, null-gated on the median.
//!
//! Substrate identical to the other `fr-simd` benches: ONE binary / ONE invocation, adjacent-pair
//! interleaving (order swapped on odd rounds), `black_box` on inputs, reps calibrated per size,
//! median of paired per-round ratios, gated on the candidate median lying outside the null
//! control's p5..p95 spread (`cv` reported, never gated).
//!
//! LZF match lengths vary, so the win is length-dependent: the bench sweeps a range of common-prefix
//! lengths (the two inputs are equal up to `len-1`, then differ) so the size-dependence is visible.
//! ORIG = `common_prefix_len_scalar` (the word loop fr-persist shipped).
//! CAND = `common_prefix_len` (runtime-dispatched, AVX2 where available).

use std::hint::black_box;
use std::time::Instant;

use fr_simd::{common_prefix_len, common_prefix_len_scalar};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.002;
const LENS: [usize; 6] = [16, 32, 64, 128, 256, 512];
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn cand(a: &[u8], b: &[u8]) -> usize {
    common_prefix_len(a, b)
}
fn base_fn(a: &[u8], b: &[u8]) -> usize {
    common_prefix_len_scalar(a, b, a.len().min(b.len()))
}

fn time(reps: usize, a: &[u8], b: &[u8], f: fn(&[u8], &[u8]) -> usize) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(a), black_box(b)));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

fn calibrate(a: &[u8], b: &[u8]) -> usize {
    let mut reps = 1usize;
    loop {
        let e = time(reps, a, b, base_fn);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 26 {
            return ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
        }
        reps *= 4;
    }
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
    println!("avx2_detected={}", std::arch::is_x86_feature_detected!("avx2"));
    println!(
        "\n{:<10} {:>9} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "prefix", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );

    for len in LENS {
        // a == b for len-1 bytes, differ at the last byte: full-length match extension.
        let mut a = vec![0u8; len];
        for (i, x) in a.iter_mut().enumerate() {
            *x = (i as u8).wrapping_mul(31).wrapping_add(7);
        }
        let mut b = a.clone();
        b[len - 1] ^= 0xff;

        // Correctness gate.
        assert_eq!(cand(&a, &b), base_fn(&a, &b), "CAND disagrees");
        assert_eq!(cand(&a, &b), len - 1);

        let reps = calibrate(&a, &b);
        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: fn(&[u8], &[u8]) -> usize, cf: fn(&[u8], &[u8]) -> usize| {
                if swap {
                    let c = time(reps, &a, &b, cf);
                    time(reps, &a, &b, bf) / c
                } else {
                    let x = time(reps, &a, &b, bf);
                    x / time(reps, &a, &b, cf)
                }
            };
            let n = pair(base_fn, base_fn);
            let s = pair(base_fn, cand);
            if round == 0 {
                continue;
            }
            nulls.push(n);
            speeds.push(s);
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
            "{:<10} {:>9} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
            format!("{len} B"),
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            speedup,
            verdict
        );
    }
}
