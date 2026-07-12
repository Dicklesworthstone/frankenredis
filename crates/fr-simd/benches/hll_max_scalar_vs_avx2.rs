//! Same-binary A/B for the HLL dense-register merge (PFMERGE / multi-key PFCOUNT): the scalar
//! `dst[i] = dst[i].max(src[i])` loop (which LLVM lowers to SSE2 `pmaxub`) vs the AVX2
//! `fr_simd::max_bytes_inplace` (`_mm256_max_epu8`) now wired into `hll_merge_registers`.
//! The real workload is a 16384-byte register array (16 KiB, ≈L1); the sweep adds L2/L3/RAM.
//!
//! ORIG = `pmaxub_replica` (scalar max loop = the pre-swap LLVM-SSE2 path).
//! CAND = `fr_simd::max_bytes_inplace` (runtime-dispatched AVX2).
//! Substrate identical to `bitop_swar_vs_avx2.rs`: ONE binary, adjacent-pair interleave (swap
//! on odd rounds), black_box, reps calibrated per size, median of ratios, null-gated.

use std::hint::black_box;
use std::time::Instant;

use fr_simd::max_bytes_inplace;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const SIZES: [usize; 4] = [16 * 1024, 64 * 1024, 512 * 1024, 4 * 1024 * 1024];
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// The pre-swap path: LLVM lowers this to SSE2 `pmaxub`.
fn pmaxub_replica(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    for i in 0..n {
        dst[i] = dst[i].max(src[i]);
    }
}

fn fill(buf: &mut [u8], mut seed: u64) {
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

fn time(reps: usize, base: &[u8], src: &[u8], f: fn(&mut [u8], &[u8])) -> f64 {
    let mut dst = base.to_vec();
    let start = Instant::now();
    for _ in 0..reps {
        dst.copy_from_slice(base);
        f(black_box(&mut dst), black_box(src));
    }
    black_box(dst[0]);
    start.elapsed().as_secs_f64()
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
    {
        let mut a = vec![0u8; 1000];
        let mut b = vec![0u8; 1000];
        fill(&mut a, 0x9e37);
        fill(&mut b, 0x2545);
        let (mut d1, mut d2) = (a.clone(), a.clone());
        pmaxub_replica(&mut d1, &b);
        max_bytes_inplace(&mut d2, &b);
        assert_eq!(d1, d2, "replica vs avx2 diverged");
    }
    println!("avx2_detected={}", cfg!(target_arch = "x86_64") && is_x86_feature_detected!("avx2"));
    println!(
        "\n{:>10} {:>7} {:>9} {:>16} {:>8} {:>10} {:>16}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );

    for &size in &SIZES {
        let mut base = vec![0u8; size];
        let mut src = vec![0u8; size];
        fill(&mut base, 0x51de_51de ^ size as u64);
        fill(&mut src, 0xf00d_f00d ^ size as u64);

        let mut reps = 1usize;
        loop {
            let e = time(reps, &base, &src, pmaxub_replica);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 22 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 4;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: fn(&mut [u8], &[u8]), cf: fn(&mut [u8], &[u8])| {
                if swap {
                    let c = time(reps, &base, &src, cf);
                    time(reps, &base, &src, bf) / c
                } else {
                    let b = time(reps, &base, &src, bf);
                    b / time(reps, &base, &src, cf)
                }
            };
            let nn = pair(pmaxub_replica, pmaxub_replica);
            let sp = pair(pmaxub_replica, max_bytes_inplace);
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
            "WIN(avx2)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        let label = if size >= 1024 * 1024 {
            format!("{} MiB", size / (1024 * 1024))
        } else {
            format!("{} KiB", size / 1024)
        };
        println!(
            "{:>10} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>16}",
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
