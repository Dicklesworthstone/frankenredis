//! Same-binary A/B for the BITPOS skip-scan kernel, gated on the null-control median.
//!
//! Identical substrate to `benches/popcount.rs` (see it for the full rationale): ONE binary, ONE
//! invocation, three slots per round with the pair position-balanced by reversing execution order
//! on odd rounds, `black_box` on input and result, reps calibrated per size to ~2 ms segments,
//! median of paired per-round ratios. The gate is the candidate median lying outside the null
//! control's p5..p95 spread — `cv` is reported but never gated, because `cv < 5%` is unreachable on
//! this shared hardware.
//!
//! The worst case for `BITPOS 1` is an all-zero bitmap with the only set bit at the very end: the
//! scanner must cross the whole buffer. That is the shape this benches.
//!
//! ORIG = `first_mismatch_byte_scalar` (the `position()` word loop the baseline build emits).
//! CAND = `first_mismatch_byte` (runtime-dispatched, AVX2 where available).

use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use fr_simd::{first_mismatch_byte, first_mismatch_byte_scalar};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.002;
const SIZES: [usize; 3] = [4 * 1024, 64 * 1024, 1024 * 1024];
const NULL_SPREAD_LO_PCT: f64 = 0.05;
const NULL_SPREAD_HI_PCT: f64 = 0.95;

fn time(reps: usize, buf: &[u8], f: fn(&[u8], u8) -> Option<usize>) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(buf), black_box(0x00)).unwrap_or(0));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

fn calibrate(buf: &[u8]) -> usize {
    let mut reps = 1usize;
    loop {
        let elapsed = time(reps, buf, first_mismatch_byte_scalar);
        if elapsed >= TARGET_SEGMENT_SECS || reps > 1 << 24 {
            let scale = (TARGET_SEGMENT_SECS / elapsed.max(1e-9)).max(1.0);
            return ((reps as f64) * scale).ceil() as usize;
        }
        reps *= 4;
    }
}

fn median_and_cv(ratios: &mut [f64]) -> (f64, f64) {
    ratios.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
    let median = ratios[ratios.len() / 2];
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    let var = ratios.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / ratios.len() as f64;
    (median, 100.0 * var.sqrt() / mean)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() -> ExitCode {
    println!("avx2_detected={}", std::arch::is_x86_feature_detected!("avx2"));
    println!(
        "\n{:<10} {:>7} {:>9} {:>9} {:>9} {:>16} {:>8} {:>10} {:>8}",
        "size", "reps", "orig ms", "cand ms", "NULL med", "null p5..p95", "null cv%", "speedup",
        "spd cv%"
    );

    let mut unfit = false;
    for size in SIZES {
        // All-zero except the last bit: BITPOS 1 must scan the whole buffer.
        let mut buf = vec![0u8; size];
        *buf.last_mut().unwrap() = 0x01;

        // Correctness gate before timing.
        let expected = first_mismatch_byte_scalar(&buf, 0x00);
        assert_eq!(first_mismatch_byte(&buf, 0x00), expected, "CAND disagrees with ORIG");
        assert_eq!(expected, Some(size - 1));

        let reps = calibrate(&buf);
        // Store the two ratios directly. Each is a pair of like-work runs measured ADJACENTLY,
        // not split by a 20x-faster arm: when the fast candidate ran between the two scalar null
        // arms, its cache/frequency wake perturbed the second one and blew the null spread to
        // [0.77, 3.38]. The pair order swaps on odd rounds so neither half sits systematically
        // first; drift divides out within each adjacent pair.
        let mut null_ratios = Vec::with_capacity(ROUNDS);
        let mut speed_ratios = Vec::with_capacity(ROUNDS);
        let mut orig_min = f64::INFINITY;
        let mut cand_min = f64::INFINITY;

        for round in 0..=ROUNDS {
            let (a, b) = if round % 2 == 0 {
                (time(reps, &buf, first_mismatch_byte_scalar), time(reps, &buf, first_mismatch_byte_scalar))
            } else {
                let b = time(reps, &buf, first_mismatch_byte_scalar);
                (time(reps, &buf, first_mismatch_byte_scalar), b)
            };
            let (o, c) = if round % 2 == 0 {
                (time(reps, &buf, first_mismatch_byte_scalar), time(reps, &buf, first_mismatch_byte))
            } else {
                let c = time(reps, &buf, first_mismatch_byte);
                (time(reps, &buf, first_mismatch_byte_scalar), c)
            };
            if round == 0 {
                continue;
            }
            null_ratios.push(a / b);
            speed_ratios.push(o / c);
            orig_min = orig_min.min(o);
            cand_min = cand_min.min(c);
        }

        let (null_median, null_cv) = median_and_cv(&mut null_ratios);
        let (speedup, speed_cv) = median_and_cv(&mut speed_ratios);
        let null_lo = percentile(&null_ratios, NULL_SPREAD_LO_PCT);
        let null_hi = percentile(&null_ratios, NULL_SPREAD_HI_PCT);

        let label = if size >= 1024 * 1024 {
            format!("{} MiB", size / (1024 * 1024))
        } else {
            format!("{} KiB", size / 1024)
        };
        println!(
            "{:<10} {:>7} {:>9.4} {:>9.4} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>8.2}",
            label,
            reps,
            orig_min * 1e3,
            cand_min * 1e3,
            null_median,
            format!("[{null_lo:.3}, {null_hi:.3}]"),
            null_cv,
            speedup,
            speed_cv
        );

        if speedup <= null_hi {
            eprintln!(
                "  INDECIDABLE at {label}: candidate median {speedup:.4} inside null spread \
                 [{null_lo:.4}, {null_hi:.4}] (median {null_median:.4})"
            );
            unfit = true;
        }
    }

    if unfit {
        eprintln!("\nA/B INDECIDABLE: candidate effect is not outside the null control's spread.");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
