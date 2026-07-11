//! Same-binary A/B for the SORT result move-not-clone (frankenredis-sortmove).
//!
//! Plain SORT materialised its sorted window with `elements[*idx].clone()` and then built the
//! reply with `BulkString(Some(el.clone()))` — TWO heap allocs per element, even though both
//! `elements` and `sliced` are dropped immediately after. `sort_generic::<true>` instead
//! `mem::take`s each window slot (the window indices are a permutation, each taken once) and
//! `into_iter`s the sliced Vec into the reply — zero element clones. Both leave BYTE-IDENTICAL
//! replies (asserted here + `sort_move_matches_clone_reference`), so the ratio isolates the two
//! eliminated per-element allocs. Numeric SORT is the sharpest case: the f64 compares are ~1ns
//! but each element is two mallocs, so the clones are a large fraction of the command.
//!
//! ORIG = `bench_sort_generic::<false>` (clone), CAND = `bench_sort_generic::<true>` (move).
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of 41 paired ratios, gated on the candidate median
//! outside the null (orig-vs-orig) p5..p95.

use std::hint::black_box;
use std::time::Instant;

use fr_command::{bench_sort_generic, dispatch_argv};
use fr_protocol::RespFrame;
use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const TS: u64 = 2;

fn seed_numeric(n: usize) -> Store {
    let mut s = Store::new();
    // Pseudo-shuffled distinct integers (deterministic), pushed as one RPUSH batch.
    let mut vals: Vec<Vec<u8>> = Vec::with_capacity(n);
    let mut x = 1u64;
    for _ in 0..n {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        vals.push(format!("{}", x % 1_000_000).into_bytes());
    }
    let mut argv = vec![b"RPUSH".to_vec(), b"nums".to_vec()];
    argv.extend(vals);
    dispatch_argv(&argv, &mut s, TS).unwrap();
    s
}

fn reply_len(f: &RespFrame) -> usize {
    match f {
        RespFrame::Array(Some(v)) => v.len(),
        _ => 0,
    }
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
        "\n{:<14} {:>7} {:>9} {:>16} {:>8} {:>13} {:>14}",
        "sort", "reps", "NULL med", "null p5..p95", "null cv%", "clone/move", "verdict"
    );

    for &n in &[128usize, 1000, 5000] {
        let argv = vec![b"SORT".to_vec(), b"nums".to_vec()];
        let mut store = seed_numeric(n);

        // Byte-exactness spot check.
        let r0 = bench_sort_generic::<false>(&argv, &mut store, TS).unwrap();
        let r1 = bench_sort_generic::<true>(&argv, &mut store, TS).unwrap();
        assert_eq!(r0, r1, "n={n}: clone vs move SORT reply diverged");

        let orig = |s: &mut Store| reply_len(&bench_sort_generic::<false>(&argv, s, TS).unwrap());
        let cand = |s: &mut Store| reply_len(&bench_sort_generic::<true>(&argv, s, TS).unwrap());
        let time = |f: &dyn Fn(&mut Store) -> usize, s: &mut Store, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(s)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };

        let mut reps = 1usize;
        loop {
            let e = time(&orig, &mut store, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 18 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 4;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: &dyn Fn(&mut Store) -> usize, cf: &dyn Fn(&mut Store) -> usize, s: &mut Store| {
                if swap {
                    let c = time(cf, s, reps);
                    time(bf, s, reps) / c
                } else {
                    let bt = time(bf, s, reps);
                    bt / time(cf, s, reps)
                }
            };
            let nn = pair(&orig, &orig, &mut store);
            let sp = pair(&orig, &cand, &mut store);
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
            "WIN(move)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<14} {:>7} {:>9.4} {:>16} {:>8.2} {:>12.3}x {:>14}",
            format!("nums_{n}"),
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            speedup,
            verdict
        );
    }
}
