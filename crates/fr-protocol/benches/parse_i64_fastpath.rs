//! Same-binary A/B for `parse_i64_strict`'s <=19-digit fast path: skip the per-digit u64-overflow
//! guards (dead for any bulk-length / multibulk-count that fits u64) vs the pre-change guarded loop.
//! Byte-identical (gated by `parse_i64_strict_fast_path_matches_guarded_ref`); the delta is the two
//! branch-predicted comparisons eliminated per digit on the hottest inbound RESP primitive.
//!
//! ORIG = `bench_parse_i64_strict::<false>` (guarded).  CAND = `::<true>` (fast, the shipped path).
//! Inputs are a realistic short-length distribution ($N bulk lengths / *N array counts).

use std::hint::black_box;
use std::time::Instant;

use fr_protocol::bench_parse_i64_strict;

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.05;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn corpus() -> Vec<Vec<u8>> {
    // Distribution weighted to the common short RESP header lengths + a few longer/negative.
    let raw: &[&str] = &[
        "1", "2", "3", "4", "5", "6", "8", "10", "12", "16", "24", "32", "64", "100", "128",
        "256", "512", "1024", "2048", "4096", "8192", "16384", "65536", "100000", "1048576",
        "-1", "-2", "42", "7", "3", "3", "3", "5", "1024", "64",
    ];
    raw.iter().map(|s| s.as_bytes().to_vec()).collect()
}

#[inline(never)]
fn run_fast(corpus: &[Vec<u8>]) -> i64 {
    let mut acc = 0i64;
    for s in corpus {
        acc = acc.wrapping_add(bench_parse_i64_strict::<true>(s).unwrap_or(0));
    }
    acc
}
#[inline(never)]
fn run_guarded(corpus: &[Vec<u8>]) -> i64 {
    let mut acc = 0i64;
    for s in corpus {
        acc = acc.wrapping_add(bench_parse_i64_strict::<false>(s).unwrap_or(0));
    }
    acc
}

fn time(reps: usize, corpus: &[Vec<u8>], f: fn(&[Vec<u8>]) -> i64) -> f64 {
    let start = Instant::now();
    let mut acc = 0i64;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(corpus)));
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
    let corpus = corpus();
    let mut reps = 1usize;
    loop {
        let e = time(reps, &corpus, run_guarded);
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
        let pair = |bf: fn(&[Vec<u8>]) -> i64, cf: fn(&[Vec<u8>]) -> i64| {
            if swap {
                let c = time(reps, &corpus, cf);
                time(reps, &corpus, bf) / c
            } else {
                let b = time(reps, &corpus, bf);
                b / time(reps, &corpus, cf)
            }
        };
        let nn = pair(run_guarded, run_guarded);
        let sp = pair(run_guarded, run_fast);
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
        "WIN(fast)"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "\n{:<14} {:>8} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "primitive", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );
    println!(
        "{:<14} {:>8} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
        "parse_i64",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
