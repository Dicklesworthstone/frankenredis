//! Ceiling A/B for a listpack-backed small-HASH repr (frankenredis-b1o02).
//!
//! Redis keeps a small hash AS its RDB listpack bytes (zero decode on RESTORE, zero encode on
//! DUMP), scanning the listpack O(n) per HGET. fr instead RE-PACKS the listpack into its own
//! PackedStrMap / CompactFieldMap on RESTORE (`hash_from_listpack_spans`) — the measured HASH
//! RESTORE gap (~0.47x; DEBUG RELOAD hash-only ~0.34x). A `HashFieldMap::Listpack` variant that
//! stored the raw bytes verbatim is a multi-day, all-or-nothing rewrite (every accessor +
//! write-promotion + DUMP + OBJECT ENCODING + byte-exact gating). This bench does NOT implement
//! it — it QUANTIFIES the RESTORE-build ceiling so the multi-day go/no-go is data-backed.
//!
//! CURRENT = `hash_from_listpack_spans` (decode + dedup-check + re-pack).
//! KEEP_LISTPACK = raw `listpack.to_vec()` (what the Listpack variant would do on RESTORE).
//! ratio current/keep = the upper bound on the RESTORE-then-never-read (MIGRATE/bulk-load) win.
//! (It does NOT model the read side: a Listpack repr scans O(n) per HGET like redis, so
//! RESTORE-then-read recovers less — same tradeoff as the hm95r lazy-spans lever.)
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of 41 paired ratios, gated on the candidate
//! median outside the null (orig-vs-orig) p5..p95.

use std::hint::black_box;
use std::time::Instant;

use fr_persist::encode_listpack_strings_blob;
use fr_store::{bench_hash_restore_current, bench_hash_restore_keep_listpack};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// A hash listpack blob of `n` field/value pairs (`w`-byte values); small => PackedStrMap path
/// (n<=128, w<=64), else the CompactFieldMap path.
fn hash_listpack(n: usize, w: usize) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = (0..n)
        .flat_map(|i| {
            let f = format!("field{i:04}").into_bytes();
            let mut v = format!("val{i:04}:").into_bytes();
            v.resize(w.max(v.len()), b'x');
            [f, v]
        })
        .collect();
    let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    encode_listpack_strings_blob(&refs).expect("encode hash listpack")
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
    // Sanity: current build sees the same field count the listpack encodes.
    for (label, blob, n) in [
        ("h8", hash_listpack(8, 16), 8usize),
        ("h128", hash_listpack(128, 16), 128),
    ] {
        assert_eq!(bench_hash_restore_current(&blob), n, "{label}: field count");
    }

    println!(
        "\n{:<14} {:>7} {:>9} {:>16} {:>8} {:>13} {:>14}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "cur/keeplp", "ceiling"
    );

    let cases: &[(&str, Vec<u8>)] = &[
        ("small_8", hash_listpack(8, 16)),
        ("small_40", hash_listpack(40, 24)),
        ("small_128", hash_listpack(128, 16)),
        ("hash_256", hash_listpack(256, 16)), // > PACKED_MAX_ENTRIES -> CompactFieldMap path
    ];

    for (label, blob) in cases {
        let orig = |b: &[u8]| bench_hash_restore_current(b);
        let cand = |b: &[u8]| bench_hash_restore_keep_listpack(b);
        let time = |f: &dyn Fn(&[u8]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(blob)));
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
            let pair = |bf: &dyn Fn(&[u8]) -> usize, cf: &dyn Fn(&[u8]) -> usize| {
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
            "WIN-ceiling"
        } else {
            "no-ceiling"
        };
        println!(
            "{:<14} {:>7} {:>9.4} {:>16} {:>8.2} {:>12.2}x {:>14}",
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
