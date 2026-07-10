//! Same-binary A/B for SCAN-style glob matching: classify-per-call (`glob_match`) vs classify-once
//! (`glob_prepare` + `PreparedGlob::matches`), null-gated on the median.
//!
//! Substrate matches the other benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated per size, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread.
//!
//! Models `SCAN MATCH <pattern>` over a keyspace: a fixed pattern matched against every key. ORIG
//! re-classifies the pattern per key (the shipped `scan_pattern_matches` path); CAND classifies once
//! and matches per key. Both return identical results (asserted before timing).

use std::hint::black_box;
use std::time::Instant;

use fr_store::{glob_match, glob_prepare};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NKEYS: usize = 20_000;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn keys() -> Vec<Vec<u8>> {
    (0..NKEYS).map(|i| format!("key:{i:08}:tag").into_bytes()).collect()
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
    let ks = keys();
    // A prefix pattern matching ~10 of 20k keys — the dominant SCAN-MATCH shape (namespace scan).
    let patterns: &[(&str, &[u8])] = &[("prefix", b"key:0001*"), ("suffix", b"*:tag"), ("general", b"key:*5:tag")];

    for (label, pat) in patterns {
        // Correctness gate.
        let prepared = glob_prepare(pat);
        let orig_hits: usize = ks.iter().filter(|k| glob_match(pat, k)).count();
        let cand_hits: usize = ks.iter().filter(|k| prepared.matches(k)).count();
        assert_eq!(orig_hits, cand_hits, "{label}: hit count diverged");

        let per_call = |ks: &[Vec<u8>]| ks.iter().filter(|k| glob_match(black_box(pat), k)).count();
        let prep = |ks: &[Vec<u8>]| {
            let m = glob_prepare(black_box(pat));
            ks.iter().filter(|k| m.matches(k)).count()
        };
        let time = |f: &dyn Fn(&[Vec<u8>]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(&ks)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&per_call, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 16 {
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
            let nn = pair(&per_call, &per_call);
            let sp = pair(&per_call, &prep);
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
            "{:<9} reps={:<6} NULL {:.4} [{:.3},{:.3}] cv {:.2}%  speedup {:.3}x  {}",
            label, reps, null_med, lo, hi, cv(&nulls), speedup, verdict
        );
    }
}
