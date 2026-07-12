//! Same-binary A/B for the 2-operand BITOP build: LLVM's SSE2 `zip().map().collect()` vs the
//! one-pass AVX2 `fr_simd::bitand_collect` now wired into `Store::bitop`'s 2-operand fast path.
//! Both allocate the result `Vec` (same shape), so the ratio isolates the compute+store width.
//! AND is representative — OR/XOR are the same kernel with a different 1-cycle lane op.
//!
//! ORIG = `zip_collect_replica` (LLVM-SSE2 one-pass collect, = the pre-swap path).
//! CAND = `fr_simd::bitand_collect` (one-pass AVX2 into uninit capacity).
//! Substrate identical to `bitop_swar_vs_avx2.rs`: ONE binary, adjacent-pair interleave (swap on
//! odd rounds), black_box, reps calibrated per size, median of ratios, null-gated.

use std::hint::black_box;
use std::time::Instant;

use fr_simd::bitand_collect;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const SIZES: [usize; 4] = [8 * 1024, 64 * 1024, 512 * 1024, 4 * 1024 * 1024];
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn zip_collect_replica(a: &[u8], b: &[u8]) -> Vec<u8> {
    let n = a.len().min(b.len());
    a[..n].iter().zip(&b[..n]).map(|(x, y)| x & y).collect()
}

fn fill(buf: &mut [u8], mut seed: u64) {
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

fn time(reps: usize, a: &[u8], b: &[u8], f: fn(&[u8], &[u8]) -> Vec<u8>) -> f64 {
    let start = Instant::now();
    let mut acc = 0u8;
    for _ in 0..reps {
        let r = f(black_box(a), black_box(b));
        acc = acc.wrapping_add(r[r.len() - 1]);
        black_box(&r);
    }
    black_box(acc);
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
        fill(&mut a, 0xa11);
        fill(&mut b, 0xb22);
        assert_eq!(zip_collect_replica(&a, &b), bitand_collect(&a, &b), "replica vs avx2 diverged");
    }
    println!("avx2_detected={}", cfg!(target_arch = "x86_64") && is_x86_feature_detected!("avx2"));
    println!(
        "\n{:>10} {:>7} {:>9} {:>16} {:>8} {:>10} {:>16}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );

    for &size in &SIZES {
        let mut a = vec![0u8; size];
        let mut b = vec![0u8; size];
        fill(&mut a, 0x51de_51de ^ size as u64);
        fill(&mut b, 0xf00d_f00d ^ size as u64);

        let mut reps = 1usize;
        loop {
            let e = time(reps, &a, &b, zip_collect_replica);
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
            let pair = |bf: fn(&[u8], &[u8]) -> Vec<u8>, cf: fn(&[u8], &[u8]) -> Vec<u8>| {
                if swap {
                    let c = time(reps, &a, &b, cf);
                    time(reps, &a, &b, bf) / c
                } else {
                    let bt = time(reps, &a, &b, bf);
                    bt / time(reps, &a, &b, cf)
                }
            };
            let nn = pair(zip_collect_replica, zip_collect_replica);
            let sp = pair(zip_collect_replica, bitand_collect);
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
