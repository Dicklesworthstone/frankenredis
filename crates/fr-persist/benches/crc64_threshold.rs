//! Crossover A/B for the `crc64_redis` pclmul-vs-table dispatch threshold, after the fold-by-4
//! upgrade (fr-simd 990cfe75c) shifted the crossover down from the fold-by-1 era.
//!
//! ORIG = `fr_persist::crc64_redis_slice_table` (the slice-by-16 table = the `< threshold` arm).
//! CAND = `fr_simd::crc64` (fold-by-4 PCLMULQDQ where available).
//! Both are byte-identical (proven by `crc64_pclmul_matches_slice_table`), so this is a pure timing
//! question: at which size does fold-by-4 overtake the table? Set the `crc64_redis` threshold at the
//! smallest size where CAND is a decidable WIN, with margin.
//!
//! Substrate = the fr-simd bench convention: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box` on inputs, reps calibrated per size, median of paired
//! per-round ratios, gated on the candidate median lying outside the null control's p5..p95 spread
//! (`cv` reported, never gated).

use std::hint::black_box;
use std::time::Instant;

use fr_persist::crc64_redis_slice_table;
use fr_simd::crc64;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
// Fine granularity around the expected crossover (a few hundred bytes) plus a couple of larger
// anchors where fold-by-4 is known to win big.
const SIZES: [usize; 12] = [128, 192, 256, 320, 384, 448, 512, 640, 768, 896, 1024, 2048];
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

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
    println!("pclmulqdq_detected={}", std::arch::is_x86_feature_detected!("pclmulqdq"));
    println!(
        "\n{:<10} {:>9} {:>9} {:>16} {:>8} {:>12} {:>12}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "fold4/table", "verdict"
    );

    for size in SIZES {
        let mut buf = vec![0u8; size];
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        for b in buf.iter_mut() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (s >> 33) as u8;
        }
        // Byte-identity guard: the two arms must agree before either is timed.
        assert_eq!(crc64(&buf), crc64_redis_slice_table(&buf), "fold4 != table at size={size}");

        let base = |d: &[u8]| crc64_redis_slice_table(d);
        let cand = |d: &[u8]| crc64(d);
        let time = |f: &dyn Fn(&[u8]) -> u64, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0u64;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(&buf)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&base, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 24 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 4;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: &dyn Fn(&[u8]) -> u64, cf: &dyn Fn(&[u8]) -> u64| {
                if swap {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let n = pair(&base, &base);
            let sp = pair(&base, &cand);
            if round == 0 {
                continue;
            }
            nulls.push(n);
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
        let label = if size >= 1024 {
            format!("{} KiB", size as f64 / 1024.0)
        } else {
            format!("{size} B")
        };
        println!(
            "{:<10} {:>9} {:>9.4} {:>16} {:>8.2} {:>11.3}x {:>12}",
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
