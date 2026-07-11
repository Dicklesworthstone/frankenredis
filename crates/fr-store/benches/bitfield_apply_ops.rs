//! Same-binary A/B for the fused BITFIELD write/mixed path (frankenredis-i229a).
//!
//! fr-command's mixed/write BITFIELD loop re-resolves the key PER op (`bitfield_get_no_stat` /
//! `bitfield_set` — one `entries.get`/`get_mut` each) where upstream `bitfieldGeneric` resolves it
//! ONCE. `Store::bitfield_apply_ops` folds all ops into one `get_mut`. ORIG = the per-op loop;
//! CAND = the fused one-lookup call. Both leave BYTE-IDENTICAL state + replies (locked by the
//! `bitfield_apply_ops_matches_per_op_reference_across_matrix` unit test), so the ratio isolates
//! the eliminated N-1 keyspace lookups (hash + hashbrown probe per op).
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of 41 paired ratios, gated on the candidate median
//! outside the null (orig-vs-orig) p5..p95.

use std::hint::black_box;
use std::time::Instant;

use fr_store::{BitfieldOp, Store};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const TS: u64 = 2;

fn set_value(i: usize) -> i64 {
    ((i * 37 + 11) % 256) as i64
}

/// `pairs` interleaved SET/GET ops at byte-aligned u8 offsets (a realistic multi-field BITFIELD).
fn make_ops(pairs: usize) -> Vec<BitfieldOp> {
    let mut v = Vec::with_capacity(pairs * 2);
    for i in 0..pairs {
        v.push(BitfieldOp::Set { offset: (i * 8) as u64, bits: 8, signed: false });
        v.push(BitfieldOp::Get { offset: (i * 8) as u64, bits: 8, signed: false });
    }
    v
}

/// ORIG: the per-op loop fr-command runs today — one store lookup per op.
fn per_op(store: &mut Store, pairs: usize) -> u64 {
    let mut acc = 0u64;
    for i in 0..pairs {
        store
            .bitfield_set(b"bf", (i * 8) as u64, 8, set_value(i), TS)
            .unwrap();
        acc = acc.wrapping_add(
            store
                .bitfield_get_no_stat(b"bf", (i * 8) as u64, 8, false, TS)
                .unwrap() as u64,
        );
    }
    acc
}

/// CAND: one fused lookup for the whole command.
fn fused(store: &mut Store, ops: &[BitfieldOp]) -> u64 {
    let r = store
        .bitfield_apply_ops(b"bf", ops, TS, |idx, _cur| Some(set_value(idx / 2)))
        .unwrap();
    r.into_iter().flatten().map(|x| x as u64).fold(0, u64::wrapping_add)
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
        "\n{:<12} {:>7} {:>9} {:>16} {:>8} {:>12} {:>14}",
        "ops", "reps", "NULL med", "null p5..p95", "null cv%", "perop/fused", "verdict"
    );

    for &pairs in &[1usize, 4, 8, 16] {
        let ops = make_ops(pairs);
        // Fresh store with the key already present (the common BITFIELD-on-existing case, where the
        // fusion saves the N-1 redundant lookups). Fixed SET values => the state is stable across
        // reps, so both arms measure the same work every iteration.
        let mut store = Store::new();
        store.set(b"bf".to_vec(), vec![0u8; pairs + 1], None, 0);
        // Byte-exactness spot check before timing.
        let a = {
            let mut s = Store::new();
            s.set(b"bf".to_vec(), vec![0u8; pairs + 1], None, 0);
            per_op(&mut s, pairs)
        };
        let b = {
            let mut s = Store::new();
            s.set(b"bf".to_vec(), vec![0u8; pairs + 1], None, 0);
            fused(&mut s, &ops)
        };
        assert_eq!(a, b, "pairs={pairs}: per-op/fused checksum diverged");

        let orig = |s: &mut Store| per_op(s, pairs);
        let cand = |s: &mut Store| fused(s, &ops);
        let time = |f: &dyn Fn(&mut Store) -> u64, s: &mut Store, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0u64;
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
            let pair = |bf: &dyn Fn(&mut Store) -> u64, cf: &dyn Fn(&mut Store) -> u64, s: &mut Store| {
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
            "WIN(fused)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<12} {:>7} {:>9.4} {:>16} {:>8.2} {:>11.3}x {:>14}",
            format!("{}op", pairs * 2),
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            speedup,
            verdict
        );
    }
}
