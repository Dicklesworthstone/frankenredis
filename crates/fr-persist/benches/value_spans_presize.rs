//! Same-binary A/B for `decode_value_spans` (the RESTORE hot path: hash/zset/set/list
//! `*_from_listpack_spans`). The spans `Vec` used to grow from empty (`Vec::new()`, ~log2(n)
//! realloc+copies); now it is pre-sized from the listpack header's element count. Both decode the
//! same blob into byte-identical spans (asserted); the delta is the elided reallocations.
//!
//! ORIG = `bench_decode_value_spans::<false>` (grow).  CAND = `::<true>` (presize, = production).
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of paired ratios, null-gated.

use std::hint::black_box;
use std::time::Instant;

use fr_persist::encode_listpack_strings_blob;
use fr_persist::listpack::bench_decode_value_spans;

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.005;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// A listpack of `n` short string entries (hash/set/zset members are the common compact case).
fn listpack(n: usize) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = (0..n).map(|i| format!("member:{i:06}").into_bytes()).collect();
    let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    encode_listpack_strings_blob(&refs).expect("encode listpack")
}

fn grow(blob: &[u8]) -> usize {
    bench_decode_value_spans::<false>(blob).expect("grow decode").len()
}
fn presize(blob: &[u8]) -> usize {
    bench_decode_value_spans::<true>(blob).expect("presize decode").len()
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
    let cases: &[(&str, usize)] = &[("n16", 16), ("n64", 64), ("n128", 128), ("n512", 512)];

    // Correctness: grow and presize decode to byte-identical spans on every shape.
    for &(label, n) in cases {
        let blob = listpack(n);
        assert_eq!(
            bench_decode_value_spans::<false>(&blob).unwrap(),
            bench_decode_value_spans::<true>(&blob).unwrap(),
            "{label}: grow/presize spans diverged"
        );
    }

    println!(
        "\n{:<8} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );

    for &(label, n) in cases {
        let blob = listpack(n);
        let time = |f: fn(&[u8]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(blob.as_slice())));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };

        let mut reps = 1usize;
        loop {
            let e = time(grow, reps);
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
            let pair = |bf: fn(&[u8]) -> usize, cf: fn(&[u8]) -> usize| {
                if swap {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let nn = pair(grow, grow);
            let sp = pair(grow, presize);
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
            "WIN(presize)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<8} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
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
