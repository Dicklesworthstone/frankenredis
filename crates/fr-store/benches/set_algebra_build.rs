//! Same-binary A/B for the set-algebra `*STORE` result build, null-gated on the median.
//!
//! Substrate matches the `fr-simd` benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box` inputs, reps calibrated per size, median of paired
//! per-round ratios, gated on the candidate median lying outside the null control's p5..p95 spread
//! (`cv` reported, never gated).
//!
//! ORIG = the pre-cc_fr build: `CompactStrSet::new()` grown incrementally (O(log n) `rehash`es).
//! CAND = the shipped build: capacity hint honored, then `shrink_to_fit`.
//! Both produce byte-identical sets; this measures only the rehash-avoidance.
//!
//! The result-size sweep mirrors set-algebra reality: SINTERSTORE's result is ≤ the smallest input,
//! so a "high overlap" build inserts near-`n` members (presize is pure win) while a "low overlap"
//! build inserts few (presize + shrink stays RAM-neutral).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
// (member count built): spans past PACKED_MAX_ENTRIES (128) into the hashtable regime.
const SIZES: [usize; 4] = [512, 2000, 5000, 20000];
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn members(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("setmember:{i:08}:common-tag").into_bytes())
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
    println!(
        "\n{:<10} {:>9} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "members", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );

    for n in SIZES {
        let ms = members(n);
        // Correctness gate: both builds must agree on the member count.
        assert_eq!(
            Store::bench_build_set_algebra_hash(&ms, true),
            Store::bench_build_set_algebra_hash(&ms, false)
        );

        let unsized_ = |m: &[Vec<u8>]| Store::bench_build_set_algebra_hash(m, false);
        let presized = |m: &[Vec<u8>]| Store::bench_build_set_algebra_hash(m, true);
        let time = |f: &dyn Fn(&[Vec<u8>]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(&ms)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&unsized_, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 20 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 4;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: &dyn Fn(&[Vec<u8>]) -> usize, cf: &dyn Fn(&[Vec<u8>]) -> usize| {
                if swap {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let nn = pair(&unsized_, &unsized_);
            let sp = pair(&unsized_, &presized);
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
            "WIN"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<10} {:>9} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
            n,
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            speedup,
            verdict
        );
    }
}
