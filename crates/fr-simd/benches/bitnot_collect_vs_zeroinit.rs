//! Same-binary A/B for BITOP NOT result construction: `vec![0u8; N]` (alloc_zeroed memset) +
//! `bitnot_into` (AVX2 overwrite), vs `bitnot_collect` (one-pass AVX2 into uninit capacity, no
//! memset). Both AVX2 for the NOT itself; the delta is the elided memset. Each iteration builds a
//! fresh result and drops it (the BITOP hot path). Byte-identical (asserted).
//!
//! ORIG = `zeroinit_then_not`.  CAND = `fr_simd::bitnot_collect`.

use std::hint::black_box;
use std::time::Instant;

use fr_simd::{bitnot_collect, bitnot_into};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const SIZES: [usize; 4] = [8 * 1024, 64 * 1024, 512 * 1024, 4 * 1024 * 1024];
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn zeroinit_then_not(src: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; src.len()];
    bitnot_into(&mut result, src);
    result
}

fn fill(buf: &mut [u8], mut seed: u64) {
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

fn time(reps: usize, src: &[u8], f: fn(&[u8]) -> Vec<u8>) -> f64 {
    let start = Instant::now();
    let mut acc = 0u8;
    for _ in 0..reps {
        let r = f(black_box(src));
        acc = acc.wrapping_add(r[r.len() - 1]).wrapping_add(r[0]);
        drop(black_box(r));
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
        let mut s = vec![0u8; 1000];
        fill(&mut s, 0xb17);
        assert_eq!(zeroinit_then_not(&s), bitnot_collect(&s), "variants diverged");
    }
    println!("avx2_detected={}", cfg!(target_arch = "x86_64") && is_x86_feature_detected!("avx2"));
    println!(
        "\n{:>10} {:>7} {:>9} {:>16} {:>8} {:>10} {:>16}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );

    for &size in &SIZES {
        let mut src = vec![0u8; size];
        fill(&mut src, 0xf00d_f00d ^ size as u64);

        let mut reps = 1usize;
        loop {
            let e = time(reps, &src, zeroinit_then_not);
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
            let pair = |bf: fn(&[u8]) -> Vec<u8>, cf: fn(&[u8]) -> Vec<u8>| {
                if swap {
                    let c = time(reps, &src, cf);
                    time(reps, &src, bf) / c
                } else {
                    let b = time(reps, &src, bf);
                    b / time(reps, &src, cf)
                }
            };
            let nn = pair(zeroinit_then_not, zeroinit_then_not);
            let sp = pair(zeroinit_then_not, bitnot_collect);
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
            "WIN(no-memset)"
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
