//! Same-binary A/B for the HLL merge accumulator (PFMERGE / multi-key PFCOUNT): `vec![0u8; N]`
//! (alloc_zeroed memset) + register-wise max of every source, vs `with_capacity(N)` where the
//! FIRST source is copied in (`extend_from_slice`) and the rest are max-folded — eliding both the
//! 16 KiB memset and the wasted first max pass. Each timed iteration builds a fresh accumulator and
//! drops it, matching the once-per-call hot path. Byte-identical (max against zeros == copy).
//!
//! ORIG = `accum_zeroed`.  CAND = `accum_fold` (with_capacity + copy-first + max-rest).

use std::hint::black_box;
use std::time::Instant;

const N: usize = 16384;
const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.006;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn max_into(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    fr_simd::max_bytes_inplace(&mut dst[..n], &src[..n]);
}

fn accum_zeroed(sources: &[Vec<u8>]) -> Vec<u8> {
    let mut merged = vec![0u8; N];
    for s in sources {
        max_into(&mut merged, s);
    }
    merged
}

fn accum_fold(sources: &[Vec<u8>]) -> Vec<u8> {
    let mut merged: Vec<u8> = Vec::with_capacity(N);
    for s in sources {
        if merged.is_empty() {
            merged.extend_from_slice(s);
        } else {
            max_into(&mut merged, s);
        }
    }
    if merged.len() < N {
        merged.resize(N, 0);
    }
    merged
}

fn fill(buf: &mut [u8], mut seed: u64) {
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = ((seed >> 40) as u8) & 0x3f;
    }
}

fn time(reps: usize, srcs: &[Vec<u8>], f: fn(&[Vec<u8>]) -> Vec<u8>) -> f64 {
    let start = Instant::now();
    let mut acc = 0u8;
    for _ in 0..reps {
        let r = f(black_box(srcs));
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

fn run(label: &str, nsrc: usize) {
    let sources: Vec<Vec<u8>> = (0..nsrc)
        .map(|i| {
            let mut v = vec![0u8; N];
            fill(&mut v, 0x484c_0000 ^ (i as u64 + 1));
            v
        })
        .collect();
    assert_eq!(accum_zeroed(&sources), accum_fold(&sources), "{label}: variants diverged");

    let mut reps = 1usize;
    loop {
        let e = time(reps, &sources, accum_zeroed);
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
        let pair = |bf: fn(&[Vec<u8>]) -> Vec<u8>, cf: fn(&[Vec<u8>]) -> Vec<u8>| {
            if swap {
                let c = time(reps, &sources, cf);
                time(reps, &sources, bf) / c
            } else {
                let b = time(reps, &sources, bf);
                b / time(reps, &sources, cf)
            }
        };
        let nn = pair(accum_zeroed, accum_zeroed);
        let sp = pair(accum_zeroed, accum_fold);
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
        "{:<10} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
        label,
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}

fn main() {
    println!("\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}", "sources", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict");
    run("2src", 2);
    run("4src", 4);
    run("8src", 8);
}
