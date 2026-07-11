//! Same-binary A/B for the list DUMP payload memoization (frankenredis-99fwc).
//!
//! Redis keeps a small list AS its RDB listpack and copies those bytes out on DUMP; fr stores a
//! ChunkedList/PackedList and RE-ENCODES the full quicklist2 wire payload (listpack nodes + CRC64)
//! on every DUMP. A repeated DUMP of an unchanged list therefore re-paid the whole re-encode. This
//! lever memoizes the payload (keyed by modification_count + list_max_listpack_size, so ANY list
//! write invalidates — locked by the `list_dump_cache_invalidates_on_every_list_mutation` unit
//! test), turning the steady-state DUMP into a `Vec::clone`.
//!
//! ORIG = `bench_dump_list_reencode` (the pre-99fwc re-encode). CAND = `bench_dump_list_cache_get`
//! (the cache-hit clone). Both emit BYTE-IDENTICAL payloads (asserted before timing), so the ratio
//! isolates the eliminated re-encode. `list_max_listpack_size` fixed at 128 (the redis default),
//! so a list of >128 entries spans multiple quicklist nodes (the re-encode the cache saves grows
//! with node count).
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of 41 paired ratios, gated on the candidate median
//! outside the null (orig-vs-orig) p5..p95.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const TS: u64 = 2;

/// A list of `n` items of `w`-byte values (mix of int-encodable and string), RPUSH-built.
fn make_list(n: usize, w: usize) -> Store {
    let mut s = Store::new();
    let items: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            if i % 3 == 0 {
                // int-encodable element (listpack stores these compactly)
                format!("{}", i * 7 + 1).into_bytes()
            } else {
                let mut v = format!("item{i:05}:").into_bytes();
                v.resize(w.max(v.len()), b'x');
                v
            }
        })
        .collect();
    s.rpush(b"l", &items, TS).unwrap();
    s
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
        "list", "reps", "NULL med", "null p5..p95", "null cv%", "reenc/clone", "verdict"
    );

    let cases: &[(&str, usize, usize)] = &[
        ("n16_w16", 16, 16),
        ("n64_w24", 64, 24),
        ("n128_w16", 128, 16),
        ("n512_w16", 512, 16), // spans several quicklist nodes -> larger re-encode
        ("n256_w64", 256, 64),
    ];

    for &(label, n, w) in cases {
        let mut store = make_list(n, w);
        // Populate the cache once (outside timing), exactly as the first real DUMP would.
        let seed = store.dump_key(b"l", TS).expect("dump populates cache");
        let reenc = store.bench_dump_list_reencode(b"l").expect("reencode");
        let cached = store.bench_dump_list_cache_get(b"l").expect("cache get");
        assert_eq!(seed, reenc, "{label}: dump_key vs reencode diverged");
        assert_eq!(reenc, cached, "{label}: reencode vs cached-clone diverged");

        let orig = |s: &Store| s.bench_dump_list_reencode(b"l").map_or(0, |v| v.len());
        let cand = |s: &Store| s.bench_dump_list_cache_get(b"l").map_or(0, |v| v.len());
        let time = |f: &dyn Fn(&Store) -> usize, s: &Store, reps: usize| -> f64 {
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
            let e = time(&orig, &store, reps);
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
            let pair = |bf: &dyn Fn(&Store) -> usize, cf: &dyn Fn(&Store) -> usize| {
                if swap {
                    let c = time(cf, &store, reps);
                    time(bf, &store, reps) / c
                } else {
                    let b = time(bf, &store, reps);
                    b / time(cf, &store, reps)
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
            "WIN(cache)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
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
