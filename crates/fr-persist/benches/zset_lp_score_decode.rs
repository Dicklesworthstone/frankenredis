//! Same-binary A/B for RDB `ZSET_LISTPACK` decode (frankenredis zsetlpscore).
//!
//! The RDB zset-listpack arm decodes `m1, score1, m2, score2, …`. Upstream stores
//! a non-integer score (`1.5`, `inf`, …) as a listpack STRING entry, so the ORIG
//! path (`decode_listpack` + a pair loop) let `decode_listpack` heap-allocate a
//! `Vec<u8>` for every such score just to `from_utf8` + `parse::<f64>` it and DROP
//! the `Vec`. `decode_zset_listpack_pairs` reads each score through the
//! allocation-free `decode_entry_raw` core instead — integer scores stay
//! `n as f64` (CrimsonHawk `788bbfd00`), string scores parse a BORROWED slice — so
//! no score `Vec` is ever allocated. Members still materialize their owned bytes.
//!
//! ORIG = `decode_zset_listpack_pairs_orig` (decode_listpack + pair-parse).
//! CAND = `decode_zset_listpack_pairs` (score alloc elided).
//! Expectation: WIN on fractional-score sets (the wasted alloc), NEUTRAL on
//! integer-score sets (both take `n as f64` — the guard).
//!
//! Substrate = the cc bench roster (listpack_decode_move.rs): ONE binary,
//! adjacent-pair interleave (swap on odd rounds), black_box, reps calibrated per
//! input, median of 41 paired ratios, gated on the candidate median outside the
//! null (orig-vs-orig) p5..p95. Both arms produce BIT-IDENTICAL pairs (asserted
//! before timing).

use std::hint::black_box;
use std::time::Instant;

use fr_persist::encode_listpack_strings_blob;
use fr_persist::listpack::{decode_zset_listpack_pairs, decode_zset_listpack_pairs_orig};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

#[derive(Clone, Copy)]
enum Kind {
    Frac,
    Int,
    Mixed,
}

/// A faithful raw zset-listpack of `n` (member, score) pairs. The production
/// listpack encoder int-encodes canonical-integer scores and string-encodes
/// fractional ones, exactly as the RDB save path does.
fn zset_blob(n: usize, kind: Kind) -> Vec<u8> {
    let mut owned: Vec<Vec<u8>> = Vec::with_capacity(n * 2);
    for i in 0..n {
        owned.push(format!("m{i:05}:tag").into_bytes());
        let score = match kind {
            Kind::Frac => format!("{}.{:03}", i, (i * 7) % 1000),
            Kind::Int => format!("{}", (i as i64) * 13 - 300),
            Kind::Mixed => {
                if i % 3 == 0 {
                    format!("{}", (i as i64) - 64)
                } else {
                    format!("{}.{:03}", i, (i * 7) % 1000)
                }
            }
        };
        owned.push(score.into_bytes());
    }
    let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    encode_listpack_strings_blob(&refs).expect("encode zset listpack")
}

fn decode_orig(blob: &[u8]) -> u64 {
    checksum(&decode_zset_listpack_pairs_orig(blob).expect("orig decode"))
}
fn decode_new(blob: &[u8]) -> u64 {
    checksum(&decode_zset_listpack_pairs(blob).expect("new decode"))
}
fn checksum(pairs: &[(Vec<u8>, f64)]) -> u64 {
    let mut acc = 0u64;
    for (m, s) in pairs {
        acc = acc.wrapping_add(m.len() as u64).wrapping_add(s.to_bits());
    }
    acc
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
    // Correctness gate: orig and new produce BIT-identical pairs on every shape.
    for (label, blob) in [
        ("frac", zset_blob(96, Kind::Frac)),
        ("int", zset_blob(96, Kind::Int)),
        ("mixed", zset_blob(96, Kind::Mixed)),
    ] {
        let a = decode_zset_listpack_pairs_orig(&blob).expect("orig");
        let b = decode_zset_listpack_pairs(&blob).expect("new");
        let abits: Vec<(Vec<u8>, u64)> = a.iter().map(|(m, s)| (m.clone(), s.to_bits())).collect();
        let bbits: Vec<(Vec<u8>, u64)> = b.iter().map(|(m, s)| (m.clone(), s.to_bits())).collect();
        assert_eq!(abits, bbits, "{label}: orig/new diverged");
    }

    println!(
        "\n{:<14} {:>7} {:>9} {:>16} {:>8} {:>11} {:>14}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "new/orig", "verdict"
    );

    let cases: &[(&str, Vec<u8>)] = &[
        ("frac_96", zset_blob(96, Kind::Frac)),
        ("frac_512", zset_blob(512, Kind::Frac)),
        ("mixed_96", zset_blob(96, Kind::Mixed)),
        ("int_96", zset_blob(96, Kind::Int)), // guard: expect neutral
    ];

    for (label, blob) in cases {
        let orig = |b: &[u8]| decode_orig(b);
        let cand = |b: &[u8]| decode_new(b);
        let time = |f: &dyn Fn(&[u8]) -> u64, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0u64;
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
            let pair = |bf: &dyn Fn(&[u8]) -> u64, cf: &dyn Fn(&[u8]) -> u64| {
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
            "WIN(elide)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
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
