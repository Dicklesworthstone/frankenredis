//! Same-binary A/B for the LPOP/RPOP count result `Vec<Vec<u8>>` construction: grow from empty
//! (`Vec::new()`) vs pre-sized `Vec::with_capacity(count.min(l.len()))`. In the real command each
//! popped element is MOVED in (a 24-byte pointer), so the cost the presize removes is the outer
//! `Vec`'s ~log2(n) realloc+copies — isolated here by pushing non-allocating empty `Vec`s.
//!
//! ORIG = `build_grow`.  CAND = `build_presize`.  Both produce a length-`n` `Vec<Vec<u8>>`.

use std::hint::black_box;
use std::time::Instant;

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

#[inline(never)]
fn build_grow(n: usize) -> Vec<Vec<u8>> {
    let mut r: Vec<Vec<u8>> = Vec::new();
    for _ in 0..n {
        r.push(black_box(Vec::<u8>::new()));
    }
    r
}

#[inline(never)]
fn build_presize(n: usize) -> Vec<Vec<u8>> {
    let mut r: Vec<Vec<u8>> = Vec::with_capacity(n);
    for _ in 0..n {
        r.push(black_box(Vec::<u8>::new()));
    }
    r
}

fn time(reps: usize, n: usize, f: fn(usize) -> Vec<Vec<u8>>) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        let r = f(black_box(n));
        acc = acc.wrapping_add(r.len());
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

fn run(label: &str, n: usize) {
    assert_eq!(build_grow(n).len(), build_presize(n).len());

    let mut reps = 1usize;
    loop {
        let e = time(reps, n, build_grow);
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
        let pair = |bf: fn(usize) -> Vec<Vec<u8>>, cf: fn(usize) -> Vec<Vec<u8>>| {
            if swap {
                let c = time(reps, n, cf);
                time(reps, n, bf) / c
            } else {
                let b = time(reps, n, bf);
                b / time(reps, n, cf)
            }
        };
        let nn = pair(build_grow, build_grow);
        let sp = pair(build_grow, build_presize);
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

fn main() {
    println!("\n{:<8} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}", "count", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict");
    run("n8", 8);
    run("n64", 64);
    run("n512", 512);
    run("n4096", 4096);
}
