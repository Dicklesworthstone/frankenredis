//! Same-binary A/B for the LZF match-tail SIMD routing (frankenredis-g9h0v follow-up).
//!
//! `lzf_compress`'s match-extension inner loop calls `common_prefix_len` on the
//! tail of every match that already matched its first 18 bytes (the SWAR fast
//! path). ORIG always runs the local scalar word loop. CAND (`lzf_match_tail_len`)
//! keeps that inlined word loop for short tails (`< 128 B`) but routes long tails
//! (`>= 128 B`, i.e. highly repetitive runs) through `fr_simd::common_prefix_len`,
//! whose AVX2 arm is BIT-identical to the word loop but 1.8–2.9x faster from 128 B.
//! Gating in fr-persist keeps the common short match on the zero-overhead inline
//! path, so the routing is Pareto-safe: never a regression, a win only where LZF
//! actually feeds long match tails (large repeated values in a DUMP payload).
//!
//! ORIG = `bench_lzf_compress::<false>`  (always-local, = production).
//! CAND = `bench_lzf_compress::<true>`   (>= 128 B tails via fr_simd AVX2).
//! Expectation: WIN on long-run payloads, INDISTINGUISHABLE on short-match /
//! text / structured payloads (the guards — must never regress). Both arms emit
//! BYTE-IDENTICAL compressed bytes (asserted before timing).
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on
//! odd rounds), black_box, reps calibrated per input, median of 41 paired ratios,
//! gated on the candidate median outside the null (orig-vs-orig) p5..p95.

use std::hint::black_box;
use std::time::Instant;

use fr_persist::bench_lzf_compress;

const ROUNDS: usize = 81;
const TARGET_SEGMENT_SECS: f64 = 0.020;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// Long repeated-run payload: `copies` back-to-back copies of a `unit`-byte pseudo
/// -random block. Each copy matches the previous one for the full `unit` bytes, so
/// LZF's match tails routinely exceed 128 B — the AVX2 win regime. Realistic of a
/// list/hash DUMP whose elements are large and near-identical.
fn repeated_runs(unit: usize, copies: usize) -> Vec<u8> {
    let mut block = Vec::with_capacity(unit);
    let mut s: u32 = 0x1234_5678;
    for _ in 0..unit {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        block.push((s >> 24) as u8);
    }
    let mut out = Vec::with_capacity(unit * copies);
    for _ in 0..copies {
        out.extend_from_slice(&block);
    }
    out
}

/// A single big run of one byte — the pure AVX2-heavy extreme (every match extends
/// to MAX_REF, tails ~245 B).
fn single_byte_run(n: usize) -> Vec<u8> {
    vec![b'x'; n]
}

/// Structured members with SHORT common prefixes (`member:00001:...`). Matches are
/// dominated by the ~14 B shared prefix, well under 128 B — a guard that the gate
/// never regresses the common short-match case.
fn structured(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 28);
    for i in 0..n {
        out.extend_from_slice(format!("member:{i:05}:payload-{:03}\n", (i * 7) % 999).as_bytes());
    }
    out
}

/// Repetitive-but-not-long English-ish text: moderate matches, typically < 128 B —
/// the realistic-corpus guard.
fn textish(target: usize) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "redis",
        "listpack", "quicklist", "compress", "value", "member", "score", "field",
    ];
    let mut out = Vec::with_capacity(target + 16);
    let mut s: u32 = 0x9e37_79b9;
    while out.len() < target {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.extend_from_slice(WORDS[(s >> 28) as usize % WORDS.len()].as_bytes());
        out.push(b' ');
    }
    out
}

fn compress_orig(p: &[u8]) -> usize {
    bench_lzf_compress::<false>(p, p.len()).map_or(0, |c| c.len())
}
fn compress_cand(p: &[u8]) -> usize {
    bench_lzf_compress::<true>(p, p.len()).map_or(0, |c| c.len())
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
    let cases: &[(&str, Vec<u8>)] = &[
        ("runs_256x24", repeated_runs(256, 24)), // long tails -> AVX2 (expect WIN)
        ("runs_512x12", repeated_runs(512, 12)), // long tails -> AVX2 (expect WIN)
        ("onebyte_8k", single_byte_run(8192)),   // AVX2 extreme (expect WIN)
        ("structured_512", structured(512)),     // short prefixes (guard: no regression)
        ("textish_8k", textish(8192)),           // moderate matches (guard: no regression)
    ];

    // Correctness gate: ORIG and CAND compress to BYTE-IDENTICAL bytes on every shape.
    for (label, p) in cases {
        let a = bench_lzf_compress::<false>(p, p.len());
        let b = bench_lzf_compress::<true>(p, p.len());
        assert_eq!(a, b, "{label}: SIMD routing changed the compressed bytes");
    }

    println!(
        "\n{:<16} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );

    for (label, p) in cases {
        let time = |f: &dyn Fn(&[u8]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(p.as_slice())));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let orig = |p: &[u8]| compress_orig(p);
        let cand = |p: &[u8]| compress_cand(p);

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
            "WIN(avx2 tail)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<16} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
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
