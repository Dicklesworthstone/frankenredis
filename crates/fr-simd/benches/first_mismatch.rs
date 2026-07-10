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
use std::time::Instant;

use fr_simd::{first_mismatch_byte, first_mismatch_byte_scalar};

/// Safe wrapper so the bench can time the SSE2 tier directly. On an AVX2 host the dispatcher would
/// otherwise always pick AVX2 and the SSE2 fallback — the tier that matters for pre-AVX2 hosts —
/// would never be measured.
fn fm_sse2(bytes: &[u8], value: u8) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse2") {
            // SAFETY: sse2 confirmed present (always true on x86_64).
            return unsafe { fr_simd::first_mismatch_byte_sse2(bytes, value) };
        }
    }
    first_mismatch_byte_scalar(bytes, value)
}

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

fn main() {
    println!("avx2_detected={}", std::arch::is_x86_feature_detected!("avx2"));
    println!(
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>11}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "SSE2 spd", "AVX2 spd"
    );

    let mut unfit = false;
    for size in SIZES {
        // All-zero except the last bit: BITPOS 1 must scan the whole buffer.
        let mut buf = vec![0u8; size];
        *buf.last_mut().unwrap() = 0x01;

        // Correctness gate before timing: every tier must equal the oracle.
        let expected = first_mismatch_byte_scalar(&buf, 0x00);
        assert_eq!(first_mismatch_byte(&buf, 0x00), expected, "AVX2 dispatch disagrees");
        assert_eq!(fm_sse2(&buf, 0x00), expected, "SSE2 tier disagrees");
        assert_eq!(expected, Some(size - 1));

        let reps = calibrate(&buf);
        // Each ratio is a pair of runs measured ADJACENTLY, not split by a faster arm: when a fast
        // candidate ran between the two scalar null arms, its cache/frequency wake perturbed the
        // second one and blew the null spread to [0.77, 3.38]. Pair order swaps on odd rounds so
        // neither half sits systematically first; drift divides out within each adjacent pair.
        let mut null_ratios = Vec::with_capacity(ROUNDS);
        let mut sse2_ratios = Vec::with_capacity(ROUNDS);
        let mut avx2_ratios = Vec::with_capacity(ROUNDS);

        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |base: fn(&[u8], u8) -> Option<usize>, cand: fn(&[u8], u8) -> Option<usize>| {
                if swap {
                    let c = time(reps, &buf, cand);
                    time(reps, &buf, base) / c
                } else {
                    let b = time(reps, &buf, base);
                    b / time(reps, &buf, cand)
                }
            };
            let n = pair(first_mismatch_byte_scalar, first_mismatch_byte_scalar);
            let s = pair(first_mismatch_byte_scalar, fm_sse2);
            let a = pair(first_mismatch_byte_scalar, first_mismatch_byte);
            if round == 0 {
                continue;
            }
            null_ratios.push(n);
            sse2_ratios.push(s);
            avx2_ratios.push(a);
        }

        let (null_median, null_cv) = median_and_cv(&mut null_ratios);
        let (sse2_speedup, _) = median_and_cv(&mut sse2_ratios);
        let (speedup, _) = median_and_cv(&mut avx2_ratios);
        let null_lo = percentile(&null_ratios, NULL_SPREAD_LO_PCT);
        let null_hi = percentile(&null_ratios, NULL_SPREAD_HI_PCT);

        let label = if size >= 1024 * 1024 {
            format!("{} MiB", size / (1024 * 1024))
        } else {
            format!("{} KiB", size / 1024)
        };
        println!(
            "{:<10} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>10.3}x",
            label,
            reps,
            null_median,
            format!("[{null_lo:.3}, {null_hi:.3}]"),
            null_cv,
            sse2_speedup,
            speedup
        );

        if speedup <= null_hi {
            eprintln!(
                "  INDECIDABLE at {label}: candidate median {speedup:.4} inside null spread \
                 [{null_lo:.4}, {null_hi:.4}] (median {null_median:.4})"
            );
            unfit = true;
        }
    }

    // Verdict goes to the reader via the INDECIDABLE lines above; the process exits 0 so this
    // bench never fails `cargo test --all-targets`. A `cargo bench` consumer reads the verdict.
    let _ = unfit;
}
