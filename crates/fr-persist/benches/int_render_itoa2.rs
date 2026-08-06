//! Same-binary A/B for i64->decimal-bytes rendering (frankenredis-tgr69/ef928/087qq).
//!
//! The integer materialization paths (packed-int decode, RDB/ziplist integer restore, listpack
//! int decode) render an i64 to its canonical decimal bytes. ORIG used `i64::to_string()
//! .into_bytes()` — the `core::fmt` Display machinery + a String alloc. The itoa2 conversion
//! (shared primitive `decimal_i64_scratch` -> `fr_protocol::write_u64_digits`, mirrored here)
//! writes digits directly into a stack buffer, then one required result `Vec`. BOTH do exactly
//! one heap alloc (the result), so this isolates the COMPUTE win (direct digit writing vs the
//! fmt machinery) — NOT an alloc elision (the render always needs its result Vec).
//!
//! ORIG = to_string; CAND = write_u64_digits scratch. verdict WIN => itoa2 render is faster.
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of 41 paired ratios. Both arms produce
//! BYTE-IDENTICAL bytes, checked before any timing runs.
//!
//! DECISION = bootstrap 95% median CI, two conditions (frankenredis-tgr69/ef928). The A/A null's
//! median CI must straddle 1.0, or the run is NULL-INADMISSIBLE and no ratio from it may be read
//! — an off-centre null means position bias lives in the instrument, and narrowing it does not
//! help. Only then must the candidate's own median CI clear 1.0 and sit outside the null's. The
//! earlier rule — candidate point median outside the null's raw p5..p95 — said nothing about how
//! well either median was determined, and its verdict flipped between rch workers for identical
//! code. `cv` is printed as provenance and is never consulted for a verdict.
//!
//! HISTORY, kept deliberately: this bench once reported ~6x by consuming both arms as
//! `render(v).len()`, letting the optimizer elide the digit loop, and three beads were closed on
//! that figure. See the 2026-08-05 ledger entry. Do not cite 6x or 2.4-3.4x.

use std::hint::black_box;
use std::time::Instant;

use fr_protocol::write_u64_digits;

const ROUNDS: usize = 41;
/// (frankenredis-tgr69/ef928) 60ms segments left the orig-vs-orig null at cv
/// 4.8-10.5% with p5..p95 as wide as [0.78, 1.16] across four runs on three
/// different rch workers — too wide to read a ~1.2x effect, and wide enough that
/// the verdict column flipped between runs for the SAME workload. vqjz1 had
/// already raised this 4ms -> 60ms without resolving it and recorded "re-run on a
/// quiescent host" as the retry predicate.
///
/// More ROUNDS cannot help: it sharpens the estimate of the null's percentiles
/// but does not narrow the distribution those percentiles describe. Only a longer
/// measured segment does, by averaging more scheduler noise into each sample. So
/// 300ms is the honest lever to reach for before declaring the effect
/// unmeasurable — it is an instrument improvement, not a relaxation of the gate,
/// which stays exactly where vqjz1 set it.
const TARGET_SEGMENT_SECS: f64 = 0.300;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// SHA-256 of the ELF actually running, reported by the process itself.
///
/// (frankenredis-tgr69/ef928) This bench has already published one figure that
/// turned out to describe something other than what it claimed, so a ratio taken
/// from it must be attributable to an identifiable build. Hashing
/// `current_exe()` from INSIDE the run is the point: a hash computed by the
/// harness outside the process could name a different binary than the one that
/// produced the numbers.
fn bench_elf_sha256() -> Result<(String, usize), String> {
    use sha2::{Digest, Sha256};

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    let bytes = std::fs::read(&executable)
        .map_err(|error| format!("could not read {}: {error}", executable.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let mut digest = String::with_capacity(64);
    for byte in hasher.finalize() {
        digest.push(char::from(HEX[usize::from(byte >> 4)]));
        digest.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok((digest, bytes.len()))
}

/// CAND: mirror of fr-persist `decimal_i64_scratch` + `decimal_i64_bytes` (the shipped itoa2 path).
fn itoa2_bytes(value: i64) -> Vec<u8> {
    let mut scratch = [0u8; 20];
    let mut start = write_u64_digits(&mut scratch, 20, value.unsigned_abs());
    if value < 0 {
        start -= 1;
        scratch[start] = b'-';
    }
    scratch[start..].to_vec()
}
/// ORIG: the pre-itoa2 fmt+alloc path.
fn to_string_bytes(value: i64) -> Vec<u8> {
    value.to_string().into_bytes()
}

/// ORIG-2: the SINGLE-DIGIT div-by-10 loop that `decimal_i64_scratch` actually
/// replaced. (frankenredis-vqjz1)
///
/// This is a different baseline from `to_string_bytes` above, and measuring the
/// right one matters: vqjz1's claim is specifically "one division per digit ->
/// two digits per division", NOT "avoid core::fmt". The to_string arm bundles
/// the Display machinery into the delta and so cannot answer vqjz1 — against
/// that baseline itoa2 would look good even if the digit loop itself were no
/// faster. Both arms here write into a stack buffer and do exactly one heap
/// alloc (the result Vec), so this isolates division count alone.
fn divloop_bytes(value: i64) -> Vec<u8> {
    let mut scratch = [0u8; 20];
    let mut end = scratch.len();
    let mut n = value.unsigned_abs();
    if n == 0 {
        end -= 1;
        scratch[end] = b'0';
    }
    while n > 0 {
        end -= 1;
        scratch[end] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    if value < 0 {
        end -= 1;
        scratch[end] = b'-';
    }
    scratch[end..].to_vec()
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

/// Bootstrap 95% confidence interval for the MEDIAN of `samples`.
///
/// (frankenredis-tgr69/ef928) The repo's decision contract is a median-CI test,
/// and this bench had no such instrument — it decided on whether the candidate
/// median fell outside the null's raw p5..p95, which says nothing about how well
/// either median is itself determined. With 41 rounds on a shared worker the
/// median is loose enough that the resulting verdict flipped between workers for
/// identical code, which is what sent an unreal figure into three bead closures.
///
/// Deterministic by construction: a fixed-seed xorshift, so re-running the same
/// ELF on the same samples reproduces the interval exactly. A bootstrap that
/// moved run to run would reintroduce the very irreproducibility it exists to
/// measure.
fn bootstrap_median_ci(samples: &[f64], seed: u64) -> (f64, f64) {
    const RESAMPLES: usize = 10_000;
    debug_assert!(!samples.is_empty());
    let n = samples.len();
    let mut state = seed | 1;
    let mut draw = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut medians = Vec::with_capacity(RESAMPLES);
    let mut resample = vec![0.0f64; n];
    for _ in 0..RESAMPLES {
        for slot in resample.iter_mut() {
            *slot = samples[(draw() % n as u64) as usize];
        }
        medians.push(median(&mut resample));
    }
    medians.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    (pct(&medians, 0.025), pct(&medians, 0.975))
}

fn main() {
    match bench_elf_sha256() {
        Ok((sha256, bytes)) => println!("bench_elf_sha256={sha256} ({bytes} bytes)"),
        // Refuse to print a table that cannot be attributed to a build. This
        // bench's history is precisely that of an unattributable number.
        Err(error) => panic!("ELF SELF-REPORT INVALID: {error}"),
    }

    // Correctness gate: byte-identical rendering across sign edges, zero, i64 extremes, widths.
    for v in [
        0i64, 1, -1, 9, -9, 10, -10, 99, -99, 100, -100, 12345, -12345, i64::MIN, i64::MAX,
        i64::MIN + 1, i64::MAX - 1, 1_000_000_000_000, -1_000_000_000_000,
    ] {
        assert_eq!(itoa2_bytes(v), to_string_bytes(v), "render diverged on {v}");
        assert_eq!(
            itoa2_bytes(v),
            divloop_bytes(v),
            "itoa2 vs div-by-10 loop diverged on {v}"
        );
    }

    // Batches of i64 spanning the digit-width distribution seen on int-heavy collections.
    fn batch(n: usize, digits: u32) -> Vec<i64> {
        let base = 10i64.pow(digits.saturating_sub(1)).max(1);
        (0..n as i64)
            .map(|i| {
                let v = base + i * 7;
                if i % 4 == 0 { -v } else { v }
            })
            .collect()
    }
    let cases: &[(&str, Vec<i64>)] = &[
        ("d1_256", batch(256, 1)),
        ("d6_256", batch(256, 6)),
        ("d18_256", batch(256, 18)),
    ];

    println!(
        "\n{:<12} {:>7} {:>9} {:>18} {:>8} {:>13} {:>18} {:>15} {:>18}",
        "workload",
        "reps",
        "NULL med",
        "null median CI95",
        "null cv%",
        "itoa2/tostr",
        "verdict",
        "itoa2/divloop",
        "verdict(vqjz1)"
    );

    for (label, vals) in cases {
        // Sink over the RENDERED BYTES, not just the length. Summing .len()
        // lets the optimizer skip materializing the digits — the length of an
        // i64's decimal form is derivable without writing them — which is how
        // this bench reported a 5.35x "win" at d1_256, where both arms perform
        // exactly one division and the true delta must be ~nil. Folding every
        // byte forces the digit loop to actually run. (frankenredis-vqjz1)
        fn fold(bytes: &[u8]) -> usize {
            bytes.iter().fold(0usize, |a, &b| a.wrapping_mul(31).wrapping_add(b as usize))
        }
        let orig = |vs: &[i64]| vs.iter().map(|&v| fold(&to_string_bytes(v))).sum::<usize>();
        let cand = |vs: &[i64]| vs.iter().map(|&v| fold(&itoa2_bytes(v))).sum::<usize>();
        let divloop = |vs: &[i64]| vs.iter().map(|&v| fold(&divloop_bytes(v))).sum::<usize>();
        let time = |f: &dyn Fn(&[i64]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(vals)));
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
        let mut divloop_speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: &dyn Fn(&[i64]) -> usize, cf: &dyn Fn(&[i64]) -> usize| {
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
            // Same interleave and swap schedule, so the div-by-10 comparison is
            // gated by the SAME null as the to_string one.
            let dv = pair(&divloop, &cand);
            if round == 0 {
                continue;
            }
            nulls.push(nn);
            speeds.push(sp);
            divloop_speeds.push(dv);
        }

        let null_med = median(&mut nulls);
        let speedup = median(&mut speeds);
        let lo = pct(&nulls, NULL_LO);
        let hi = pct(&nulls, NULL_HI);
        let divloop_speedup = median(&mut divloop_speeds);

        // MEDIAN-CI DECISION. Two independent conditions, both required.
        //
        // ADMISSIBILITY is a property of the null alone: an A/A control compares
        // a binary against ITSELF, so its median CI must straddle 1.0. A null CI
        // that excludes 1.0 means position/ordering bias lives inside the
        // instrument, and no candidate ratio measured through it can be read --
        // narrowness does not rescue an off-centre null.
        //
        // SEPARATION then requires the candidate's own median CI to clear 1.0
        // and to sit outside the null's, so a verdict states that the two
        // medians are resolved apart, not merely that two point estimates
        // differ. CV is printed as provenance and is not consulted here.
        let (null_ci_lo, null_ci_hi) = bootstrap_median_ci(&nulls, 0x9E3779B97F4A7C15);
        let null_admissible = null_ci_lo <= 1.0 && null_ci_hi >= 1.0;
        // Decides on the sample's median CI, never on the printed point estimate
        // — that point estimate is provenance, the interval is the instrument.
        let verdict = |samples: &[f64]| {
            if !null_admissible {
                return "NULL-INADMISSIBLE";
            }
            let (ci_lo, ci_hi) = bootstrap_median_ci(samples, 0xD1B54A32D192ED03);
            if ci_lo > 1.0 && ci_lo > null_ci_hi {
                "WIN(itoa2)"
            } else if ci_hi < 1.0 && ci_hi < null_ci_lo {
                "REGRESSION"
            } else {
                "indistinguishable"
            }
        };
        let _ = (lo, hi);
        println!(
            "{:<12} {:>7} {:>9.4} {:>18} {:>8.2} {:>12.3}x {:>18} {:>14.3}x {:>18}",
            label,
            reps,
            null_med,
            format!("[{null_ci_lo:.4}, {null_ci_hi:.4}]"),
            cv(&nulls),
            speedup,
            verdict(&speeds),
            divloop_speedup,
            verdict(&divloop_speeds)
        );
    }
}
