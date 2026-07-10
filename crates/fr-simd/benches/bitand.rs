//! Same-binary A/B for the BITOP AND kernel, null-gated on the median.
//!
//! Substrate identical to the other `fr-simd` benches: ONE binary / ONE invocation, adjacent-pair
//! interleaving with order swapped on odd rounds, `black_box` on inputs, reps calibrated per size,
//! median of paired per-round ratios, gated on the candidate median lying outside the null
//! control's p5..p95 spread (`cv` reported, never gated).
//!
//! BITOP is a streaming read-read-write, so — unlike the compute-bound BITCOUNT — a "win" here is
//! genuinely in question: at sizes that exceed cache it can be bandwidth-bound, where AVX2's wider
//! loads buy little. The bench sweeps L1/L2/L3-ish sizes so the size-dependence is visible.
//!
//! ORIG = `bitand_inplace_scalar` (LLVM's SSE2-vectorized word loop on x86_64).
//! CAND = `bitand_inplace` (runtime-dispatched, AVX2 where available).

use std::hint::black_box;
use std::time::Instant;

use fr_simd::{bitand_inplace, bitand_inplace_scalar};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.002;
const SIZES: [usize; 4] = [8 * 1024, 64 * 1024, 512 * 1024, 4 * 1024 * 1024];
const NULL_SPREAD_HI_PCT: f64 = 0.95;
const NULL_SPREAD_LO_PCT: f64 = 0.05;

fn fill(buf: &mut [u8], mut seed: u64) {
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

/// Time `reps` applications of `f(dst, src)`. `dst` is refilled each call so the AND is not
/// idempotent-collapsed to a no-op after the first pass.
fn time(reps: usize, base: &[u8], src: &[u8], f: fn(&mut [u8], &[u8])) -> f64 {
    let mut dst = base.to_vec();
    let start = Instant::now();
    for _ in 0..reps {
        dst.copy_from_slice(base);
        f(black_box(&mut dst), black_box(src));
    }
    black_box(dst[0]);
    // Subtract the copy_from_slice overhead by not counting it? It is identical for both arms and
    // divides out in the ratio, so leave it in — the pair is what matters.
    start.elapsed().as_secs_f64()
}

fn calibrate(base: &[u8], src: &[u8]) -> usize {
    let mut reps = 1usize;
    loop {
        let e = time(reps, base, src, bitand_inplace_scalar);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 24 {
            return ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
        }
        reps *= 4;
    }
}

fn median(ratios: &mut [f64]) -> f64 {
    ratios.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    ratios[ratios.len() / 2]
}

fn cv(r: &[f64]) -> f64 {
    let m = r.iter().sum::<f64>() / r.len() as f64;
    100.0 * (r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / r.len() as f64).sqrt() / m
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    println!("avx2_detected={}", std::arch::is_x86_feature_detected!("avx2"));
    println!(
        "\n{:<10} {:>8} {:>9} {:>16} {:>8} {:>9} {:>12}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );

    for size in SIZES {
        let mut base = vec![0u8; size];
        let mut src = vec![0u8; size];
        fill(&mut base, 0xa5a5_5a5a_c3c3_3c3c);
        fill(&mut src, 0x1234_5678_9abc_def0);

        // Correctness gate.
        let mut d1 = base.clone();
        let mut d2 = base.clone();
        bitand_inplace(&mut d1, &src);
        bitand_inplace_scalar(&mut d2, &src);
        assert_eq!(d1, d2, "CAND disagrees with ORIG");

        let reps = calibrate(&base, &src);
        let mut null_ratios = Vec::with_capacity(ROUNDS);
        let mut speed_ratios = Vec::with_capacity(ROUNDS);

        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |b: fn(&mut [u8], &[u8]), c: fn(&mut [u8], &[u8])| {
                if swap {
                    let cc = time(reps, &base, &src, c);
                    time(reps, &base, &src, b) / cc
                } else {
                    let bb = time(reps, &base, &src, b);
                    bb / time(reps, &base, &src, c)
                }
            };
            let n = pair(bitand_inplace_scalar, bitand_inplace_scalar);
            let s = pair(bitand_inplace_scalar, bitand_inplace);
            if round == 0 {
                continue;
            }
            null_ratios.push(n);
            speed_ratios.push(s);
        }

        let null_med = median(&mut null_ratios);
        let speedup = median(&mut speed_ratios);
        let null_lo = percentile(&null_ratios, NULL_SPREAD_LO_PCT);
        let null_hi = percentile(&null_ratios, NULL_SPREAD_HI_PCT);

        let label = if size >= 1024 * 1024 {
            format!("{} MiB", size / (1024 * 1024))
        } else {
            format!("{} KiB", size / 1024)
        };
        // A WIN must be BOTH a real speedup (>1.0) AND outside the null control's upper spread —
        // otherwise a 0.997x reading that merely clears a null p95 of 0.997 falsely reads "WIN".
        let verdict = if speedup > 1.0 && speedup > null_hi {
            "WIN"
        } else if speedup < 1.0 && speedup < null_lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<10} {:>8} {:>9.4} {:>16} {:>8.2} {:>8.3}x {:>12}",
            label,
            reps,
            null_med,
            format!("[{null_lo:.3}, {null_hi:.3}]"),
            cv(&null_ratios),
            speedup,
            verdict
        );
    }
}
