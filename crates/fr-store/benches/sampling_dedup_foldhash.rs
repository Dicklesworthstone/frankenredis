//! Same-binary A/B for the distinct-index dedup loop shared by SRANDMEMBER/HRANDFIELD/ZRANDMEMBER
//! `COUNT` (the `picked` set in `srandmember_count` / `hrandfield_count` / their borrow-scan twins).
//! The loop draws `next_rand() % len` and dedups into a set until `n` distinct indices are chosen.
//! ORIG used `HashSet::with_capacity(n)` — std default `RandomState` = **SipHash** (a cryptographic
//! hash) computed on every `insert`. CAND uses `foldhash::quality::RandomState` (the same hasher the
//! keyspace `entries` map already uses). The dedup is BY VALUE, so the resulting index sequence is
//! byte-identical for the same draw stream — only the hasher differs (asserted below).
//!
//! This isolates the hasher swap on the exact loop; the end-to-end RANDMEMBER share is smaller (the
//! command also materializes the sampled members). ORIG = SipHash, CAND = foldhash; WIN => foldhash
//! dedup is faster. Substrate mirrors the cc bench roster (one binary, interleave, median-of-61,
//! candidate median gated outside the orig-vs-orig null p5..p95).

use std::collections::HashSet;
use std::hint::black_box;
use std::time::Instant;

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.02;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// SplitMix64 — a deterministic stand-in for the Store's `next_rand()` so both arms draw the
/// identical index stream (the real dedup is hasher-independent; this makes the A/B exact).
#[inline]
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[inline(never)]
fn dedup_siphash(n: usize, len: usize, seed: u64) -> Vec<usize> {
    let mut st = seed;
    let mut idxs = Vec::with_capacity(n);
    let mut picked: HashSet<usize> = HashSet::with_capacity(n);
    while idxs.len() < n {
        let idx = (next(&mut st) as usize) % len;
        if picked.insert(idx) {
            idxs.push(idx);
        }
    }
    idxs
}

#[inline(never)]
fn dedup_foldhash(n: usize, len: usize, seed: u64) -> Vec<usize> {
    let mut st = seed;
    let mut idxs = Vec::with_capacity(n);
    let mut picked: HashSet<usize, foldhash::quality::RandomState> =
        HashSet::with_capacity_and_hasher(n, foldhash::quality::RandomState::default());
    while idxs.len() < n {
        let idx = (next(&mut st) as usize) % len;
        if picked.insert(idx) {
            idxs.push(idx);
        }
    }
    idxs
}

fn time(reps: usize, f: fn(usize, usize, u64) -> Vec<usize>, n: usize, len: usize) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for r in 0..reps {
        // Vary the seed per rep so we exercise many draw streams; both arms use the SAME seed.
        let v = f(n, len, black_box(0x1234_5678u64 ^ r as u64));
        acc = acc.wrapping_add(v.iter().copied().sum::<usize>());
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

fn bench(label: &str, n: usize, len: usize) {
    // Correctness: identical draw stream ⇒ identical dedup output regardless of hasher.
    for r in 0..8u64 {
        let seed = 0x1234_5678u64 ^ r;
        assert_eq!(
            dedup_siphash(n, len, seed),
            dedup_foldhash(n, len, seed),
            "hasher must not change the sampled index sequence"
        );
    }

    let mut reps = 1usize;
    loop {
        let e = time(reps, dedup_siphash, n, len);
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
        let pair = |bf: fn(usize, usize, u64) -> Vec<usize>, cf: fn(usize, usize, u64) -> Vec<usize>| {
            if swap {
                let c = time(reps, cf, n, len);
                time(reps, bf, n, len) / c
            } else {
                let b = time(reps, bf, n, len);
                b / time(reps, cf, n, len)
            }
        };
        let nn = pair(dedup_siphash, dedup_siphash);
        let sp = pair(dedup_siphash, dedup_foldhash);
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
        "{:<16} {:>8} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
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
    println!(
        "\n{:<16} {:>8} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n/len", "reps", "NULL med", "null p5..p95", "null cv%", "fold/sip", "verdict"
    );
    // Sparse (few collisions) → dedup ~= n inserts.
    bench("n16_len100k", 16, 100_000);
    bench("n256_len100k", 256, 100_000);
    bench("n1000_len100k", 1000, 100_000);
    // Dense (n near len/2) → heavy redraw, many inserts+probes.
    bench("n511_len1024", 511, 1024);
    bench("n500_len2048", 500, 2048);
}
