//! Same-binary A/B for the quicklist2 DUMP-fallback packed-node encode (frankenredis-k1wcp).
//!
//! `encode_dump_quicklist2`'s fallback (the COMMON small-list DUMP path — small lists are
//! `ListRepr::Packed`, so they miss both the quicklist_packed_nodes and retained_listpack_chunks
//! fast paths) currently DIRECT-EMITs each item into a fixed-8192-presized buffer then
//! `finish_listpack_entries` (k1wcp `3986ca7ad`, never A/B'd). Its fr-persist sibling
//! (`encode_compact_list_quicklist2`) used the identical direct-emit and was measured 1.0659x
//! SLOWER on the focused encoder gate and REVERTED to the BUFFERED roster path (b89361c13).
//! This gate checks whether the fr-store DUMP fallback carries the same latent regression.
//!
//! ORIG = DIRECT (current production): stream items -> finish_listpack_entries.
//! CAND = BUFFERED (the proposed revert-to-parity): Vec<&[u8]> roster -> encode_listpack_strings.
//! verdict WIN  => buffered (revert) is faster => k1wcp is a regression, ship the revert.
//! verdict REGRESSION => direct is faster => k1wcp's direct-emit is correct here, keep it.
//!
//! Substrate = the cc bench roster (set_listpack_dump.rs / quicklist_encode.rs): ONE binary,
//! adjacent-pair interleave (swap on odd rounds), black_box, reps calibrated per input, median
//! of 41 paired ratios, gated on the candidate median outside the null (orig-vs-orig) p5..p95.
//! Both arms emit BYTE-IDENTICAL listpacks (asserted before timing).

use std::hint::black_box;
use std::time::Instant;

use fr_store::{bench_quicklist_node_buffered, bench_quicklist_node_direct};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// `n` short non-integer string items (~8 bytes; a typical small-list packed node).
fn str_items(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("item{i:04}").into_bytes()).collect()
}
/// `n` integer items (redis-benchmark list members render as canonical decimals).
fn int_items(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| ((i as i64) * 977 - 13).to_string().into_bytes()).collect()
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
        "\n{:<14} {:>7} {:>9} {:>16} {:>8} {:>11} {:>14}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "buf/dir", "verdict"
    );

    let cases: &[(&str, Vec<Vec<u8>>)] = &[
        ("str_8", str_items(8)),
        ("str_32", str_items(32)),
        ("str_128", str_items(128)),
        ("int_128", int_items(128)),
    ];

    for (label, items) in cases {
        // Correctness gate: both strategies produce byte-identical listpack node bytes.
        assert_eq!(
            bench_quicklist_node_direct(items),
            bench_quicklist_node_buffered(items),
            "{label}: direct/buffered listpack diverged"
        );

        // orig = DIRECT (current prod); cand = BUFFERED (proposed revert). speedup = dir/buf.
        let orig = |it: &[Vec<u8>]| bench_quicklist_node_direct(it).map_or(0, |v| v.len());
        let cand = |it: &[Vec<u8>]| bench_quicklist_node_buffered(it).map_or(0, |v| v.len());
        let time = |f: &dyn Fn(&[Vec<u8>]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(items)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&orig, reps);
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
            let pair = |bf: &dyn Fn(&[Vec<u8>]) -> usize, cf: &dyn Fn(&[Vec<u8>]) -> usize| {
                if swap {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let nn = pair(&orig, &orig);
            let sp = pair(&orig, &cand);
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
            "WIN(revert)"
        } else if speedup < 1.0 && speedup < lo {
            "keep-direct"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<14} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>14}",
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
