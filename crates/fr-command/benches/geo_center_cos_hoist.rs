//! NEGATIVE EVIDENCE: hoisting the GEOSEARCH search-center latitude cosine out of the per-candidate
//! haversine loop is a NO-OP — LLVM's LICM already does it.
//!
//! `geo_collect_candidate` runs once per member that survives the lat/lon bbox pre-filter, each
//! calling `geo_distance_m(center_lon, center_lat, lon, lat)`, which computes the libm
//! `center_lat.to_radians().cos()` — a value CONSTANT across the whole search. Hoisting that cosine
//! to a once-per-search precompute LOOKS like a win. It is not: geo_distance_m inlines into the
//! monomorphised candidate loop and LLVM lifts the invariant cosine on its own.
//!
//! This A/B sums the haversine over a fixed candidate set from one center, per-candidate cos
//! (`hoist = false`) vs precomputed cos (`hoist = true`). Measured with `perf stat -e instructions:u`
//! over the single-arm mode (`ref`/`cand`, 1000×4096 = 4.1M haversines/arm): **914.15M instructions
//! for BOTH arms** — a 1.000x ratio, indistinguishable. Checksums are bit-identical (the two arms
//! agree to the last bit). Conclusion: the production `geo_collect_candidate` was left calling
//! `geo_distance_m` directly; only this harness + `geo_distance_m_center_cos` remain, to document the
//! null so the manual hoist is not re-attempted.
//!
//! Single-arm perf-stat: `perf stat -e instructions:u <bench-bin> ref` vs `<bench-bin> cand`.

use std::hint::black_box;
use std::time::Instant;

use fr_command::bench_geo_center_cos_distance_sum;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
// A realistic bbox survivor count for a dense GEOSEARCH BYRADIUS.
const CANDIDATES: usize = 4096;
const CENTER_LON: f64 = 12.3456;
const CENTER_LAT: f64 = 41.9028; // Rome-ish; a mid-latitude center where cos != 1.

/// Deterministic candidate coordinates clustered near the center (as bbox survivors are), avoiding
/// the exact lon-equal (v==0) fast path so every candidate exercises the full cos-bearing haversine.
fn candidates() -> Vec<(f64, f64)> {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..CANDIDATES)
        .map(|_| {
            // ±0.5° box around the center (a few tens of km), lon offset never exactly zero.
            let lon = CENTER_LON + (next() - 0.5) + 1e-6;
            let lat = CENTER_LAT + (next() - 0.5);
            (lon, lat)
        })
        .collect()
}

fn timed(hoist: bool, cands: &[(f64, f64)], reps: usize) -> f64 {
    let start = Instant::now();
    let mut acc = 0.0f64;
    for i in 0..reps {
        // Vary the center by a per-rep jitter (geographically negligible, distinct f64 each rep):
        // otherwise the center + candidates are loop-invariant and LLVM hoists the entire per-arm
        // computation out of the timing loop (both arms then collapse to one evaluation → false
        // 1.0x). Both arms get the SAME jitter, so the comparison stays fair.
        let jitter = (i % 4096) as f64 * 1e-6;
        acc += bench_geo_center_cos_distance_sum(
            CENTER_LON + jitter,
            CENTER_LAT + jitter,
            black_box(cands),
            hoist,
        );
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

fn main() {
    let cands = candidates();

    // Single-arm mode for `perf stat -e instructions:u`: run ONE arm in a tight loop so an external
    // instruction count can compare arms directly (wallclock cannot resolve one cos/candidate).
    let arg = std::env::args().nth(1);
    if let Some(a) = arg.as_deref() {
        if a == "ref" || a == "cand" {
            let hoist = a == "cand";
            let mut acc = 0.0f64;
            for i in 0..1_000usize {
                let jitter = (i % 4096) as f64 * 1e-6;
                acc += bench_geo_center_cos_distance_sum(
                    CENTER_LON + jitter,
                    CENTER_LAT + jitter,
                    black_box(&cands),
                    hoist,
                );
            }
            println!("{a} checksum={:.6}", black_box(acc));
            return;
        }
    }

    // Correctness: the two arms must be bit-identical sums.
    let sum_ref = bench_geo_center_cos_distance_sum(CENTER_LON, CENTER_LAT, &cands, false);
    let sum_cand = bench_geo_center_cos_distance_sum(CENTER_LON, CENTER_LAT, &cands, true);
    assert_eq!(
        sum_ref.to_bits(),
        sum_cand.to_bits(),
        "hoisted-cos distance sum must be bit-identical to per-candidate cos"
    );

    let mut reps = 1usize;
    loop {
        let e = timed(false, &cands, reps);
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
        let nn = if swap {
            let c = timed(false, &cands, reps);
            timed(false, &cands, reps) / c
        } else {
            let b = timed(false, &cands, reps);
            b / timed(false, &cands, reps)
        };
        let sp = if swap {
            let c = timed(true, &cands, reps);
            timed(false, &cands, reps) / c
        } else {
            let b = timed(false, &cands, reps);
            b / timed(true, &cands, reps)
        };
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
        "\n{:<24} {:>7} {:>8} {:>9} {:>16} {:>8} {:>10} {:>14}",
        "op", "cands", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    println!(
        "{:<24} {:>7} {:>8} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>14}",
        "geo_center_cos_hoist",
        CANDIDATES,
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
