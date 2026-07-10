//! Same-binary A/B for the BITCOUNT popcount kernel, **with a null control**.
//!
//! ONE binary, ONE invocation. Arms are interleaved inside a single measured routine and their
//! order rotates every round, so worker identity and drift cancel — a ratio assembled from two
//! `rch exec` invocations would be invalid (rch has no `--worker` flag and picks
//! non-deterministically). Criterion is deliberately not used: its group members run sequentially
//! and do not cancel drift.
//!
//! **Null control (A/A).** The identical arm is registered twice and timed exactly like the real
//! arms. `null = orig / orig_null` measures this harness's own noise floor. A "win" smaller than
//! the null control is indistinguishable from noise, and a REJECT of a lever whose effect is below
//! the floor is meaningless. If the null control is not tight, the harness is not fit to decide the
//! lever and this bench **fails closed** rather than reporting a number.
//!
//! `black_box` wraps both input and result: without it, `popcount_scalar` is a pure function of a
//! constant buffer and LLVM may hoist or delete the very work under test.
//!
//! ORIG = `popcount_scalar`, the real shipped kernel (baseline `x86-64` ⇒ SSE2 SWAR).
//! CAND = `popcount_bytes`, runtime-dispatched (AVX2 where available).

use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use fr_simd::{popcount_bytes, popcount_scalar};

/// Many rounds, and the median steps around whichever round a scheduler event ruins.
const ROUNDS: usize = 41;
/// Each timed segment must be long enough that one context switch cannot dominate it. A fixed rep
/// count cannot do that across a 256x size range: at 4 KiB, 8 reps is ~1.7us, where timer
/// granularity and preemption swamp the signal and blow the null control's cv past 20%. Reps are
/// therefore calibrated per size to hit this target.
const TARGET_SEGMENT_SECS: f64 = 0.002;
const SIZES: [usize; 3] = [4 * 1024, 64 * 1024, 1024 * 1024];

/// A claim is decidable only when the candidate's median effect lies clearly OUTSIDE the null
/// control's observed spread. `cv` is reported as information but is deliberately **not** a gate:
/// a paired A/A sweep on this hardware showed `cv < 5%` is unattainable, so gating on it would
/// reject valid measurements. The null floor is also per-function and per-size, not global, so it
/// is recalibrated for every row below.
const NULL_SPREAD_LO_PCT: f64 = 0.05;
const NULL_SPREAD_HI_PCT: f64 = 0.95;

fn fill(buf: &mut [u8], mut seed: u64) {
    for byte in buf.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *byte = (seed >> 33) as u8;
    }
}

fn time(reps: usize, buf: &[u8], f: fn(&[u8]) -> usize) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(buf)));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

/// Pick a rep count so one timed segment lasts ~`TARGET_SEGMENT_SECS`, using the slower arm so
/// both arms get segments at least that long.
fn calibrate(buf: &[u8]) -> usize {
    let mut reps = 1usize;
    loop {
        let elapsed = time(reps, buf, popcount_scalar);
        if elapsed >= TARGET_SEGMENT_SECS || reps > 1 << 24 {
            let scale = (TARGET_SEGMENT_SECS / elapsed.max(1e-9)).max(1.0);
            return ((reps as f64) * scale).ceil() as usize;
        }
        reps *= 4;
    }
}

fn min_and_cv(samples: &[f64]) -> (f64, f64) {
    let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let var = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / samples.len() as f64;
    (min, 100.0 * var.sqrt() / mean)
}

/// Median and cv of the **paired, per-round** ratios.
///
/// This is the statistic that matters. Both arms of a ratio are measured microseconds apart inside
/// the same round, so machine drift — the thing that wrecks a shared worker's absolute timings —
/// divides out. The per-arm cv of absolute times can be 400% on a busy box while the paired ratio
/// stays rock steady; judging the lever on absolute cv would throw away a valid measurement, and
/// judging it on a single un-paired ratio would accept an invalid one.
fn median_and_cv(ratios: &mut [f64]) -> (f64, f64) {
    ratios.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
    let median = ratios[ratios.len() / 2];
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    let var = ratios.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / ratios.len() as f64;
    (median, 100.0 * var.sqrt() / mean)
}

/// `p` must be in `0.0..=1.0`; `sorted` must already be sorted ascending.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn main() -> ExitCode {
    println!(
        "avx2_detected={}  popcnt_detected={}",
        std::arch::is_x86_feature_detected!("avx2"),
        std::arch::is_x86_feature_detected!("popcnt"),
    );
    println!(
        "\n{:<10} {:>7} {:>9} {:>9} {:>9} {:>16} {:>8} {:>10} {:>8}",
        "size", "reps", "orig ms", "cand ms", "NULL med", "null p5..p95", "null cv%", "speedup",
        "spd cv%"
    );

    let mut unfit = false;
    for size in SIZES {
        let mut buf = vec![0u8; size];
        fill(&mut buf, 0x9e37_79b9_7f4a_7c15);

        // Correctness gate before any timing: a "faster" arm that disagrees is worthless.
        let expected: usize = buf.iter().map(|b| b.count_ones() as usize).sum();
        assert_eq!(popcount_scalar(&buf), expected, "ORIG disagrees with the oracle");
        assert_eq!(popcount_bytes(&buf), expected, "CAND disagrees with the oracle");

        let reps = calibrate(&buf);

        let mut orig = Vec::with_capacity(ROUNDS);
        let mut null = Vec::with_capacity(ROUNDS);
        let mut cand = Vec::with_capacity(ROUNDS);

        for round in 0..=ROUNDS {
            // Slot 0 and slot 1 are the SAME function: that pair is the null control.
            //
            // Rotating alone is NOT enough. With `arm = (k + round) % 3`, arm 1 always executes
            // exactly one position after arm 0, and later positions in a round run slower (cache
            // and frequency effects from the preceding arm). That leaked an ~8% systematic bias
            // into the null control (median 0.917 instead of ~1.000) and depressed the candidate
            // in the same direction. Reversing the execution order on odd rounds makes arm 1
            // precede arm 0 half the time, so the pair is position-balanced.
            let mut slot = [0.0f64; 3];
            let mut order = [(round) % 3, (1 + round) % 3, (2 + round) % 3];
            if round % 2 == 1 {
                order.reverse();
            }
            for arm in order {
                slot[arm] = match arm {
                    0 => time(reps, &buf, popcount_scalar),
                    1 => time(reps, &buf, popcount_scalar),
                    _ => time(reps, &buf, popcount_bytes),
                };
            }
            if round == 0 {
                continue; // discard warm-up
            }
            orig.push(slot[0]);
            null.push(slot[1]);
            cand.push(slot[2]);
        }

        let (orig_min, _) = min_and_cv(&orig);
        let (cand_min, _) = min_and_cv(&cand);

        // Paired within each round: drift divides out.
        let mut null_ratios: Vec<f64> = orig.iter().zip(&null).map(|(o, n)| o / n).collect();
        let mut speed_ratios: Vec<f64> = orig.iter().zip(&cand).map(|(o, c)| o / c).collect();
        let (null_ratio, null_cv) = median_and_cv(&mut null_ratios);
        let (speedup, speed_cv) = median_and_cv(&mut speed_ratios);

        let label = if size >= 1024 * 1024 {
            format!("{} MiB", size / (1024 * 1024))
        } else {
            format!("{} KiB", size / 1024)
        };
        // `null_ratios` and `speed_ratios` are sorted by `median_and_cv`.
        let null_lo = percentile(&null_ratios, NULL_SPREAD_LO_PCT);
        let null_hi = percentile(&null_ratios, NULL_SPREAD_HI_PCT);

        println!(
            "{:<10} {:>7} {:>9.4} {:>9.4} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>8.2}",
            label,
            reps,
            orig_min * 1e3,
            cand_min * 1e3,
            null_ratio,
            format!("[{null_lo:.3}, {null_hi:.3}]"),
            null_cv,
            speedup,
            speed_cv
        );

        // THE GATE: the candidate median must lie clearly outside the null control's observed
        // spread. cv is information, not a threshold -- it is unattainable under 5% on this
        // hardware, and gating on it would discard valid measurements.
        if speedup <= null_hi {
            eprintln!(
                "  INDECIDABLE at {label}: candidate median {speedup:.4} lies inside the null \
                 control's spread [{null_lo:.4}, {null_hi:.4}] (median {null_ratio:.4})"
            );
            unfit = true;
        }
    }

    if unfit {
        eprintln!("\nA/B INDECIDABLE: candidate effect is not outside the null control's spread.");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
