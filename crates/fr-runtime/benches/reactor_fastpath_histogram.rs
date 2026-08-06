//! Same-binary A/B for command-histogram recording on the reactor GET fast path.
//! (frankenredis-ktcqz)
//!
//! THE QUESTION. `execute_shared_nothing_get_into` recorded no histogram entry, so
//! INFO commandstats and latencystats came back EMPTY for the commands that
//! dominate a real workload -- measured, not assumed: 300 GETs through an
//! 8-worker reactor produced a bare `# Commandstats` header with no rows.
//! Recording needs a clock read pair per command on the hottest path in the
//! server, which is exactly the path the per-core reactor exists to make fast, so
//! whether to record is a MEASUREMENT, not a preference.
//!
//! THE ARMS ARE A REAL CONFIG KNOB, not a synthetic toggle. `latency-tracking`
//! defaults to `yes` upstream and in fr, and the non-sharded borrowed fast paths
//! already honour it. ORIG = tracking off (one predictable branch), CAND =
//! tracking on (clock pair + histogram record). So the measured delta IS the cost
//! a default deployment pays, and the "off" arm is a supported configuration
//! rather than a code variant that ships to nobody.
//!
//! SUBSTRATE, inherited from the corrected int_render_itoa2 roster: ONE binary
//! holding both arms, arm order drawn PER PAIR from a fixed-seed PRNG (strict
//! alternation aliases with periodic host drift), reps calibrated per input,
//! median of 41 paired ratios.
//!
//! DECISION = bootstrap 95% median CI with an ADMISSIBILITY GUARD. The A/A null's
//! own median CI must straddle 1.0, or the row is NULL-INADMISSIBLE and no ratio
//! from it may be read -- an A/A control compares a binary against itself, so a
//! CI excluding 1.0 condemns the run rather than the candidate. `cv` is printed
//! as provenance and is never consulted for a verdict.

use std::hint::black_box;
use std::time::Instant;

use fr_config::RuntimePolicy;
use fr_runtime::Runtime;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.300;

/// SHA-256 of the ELF actually running, reported by the process itself, so a
/// recorded ratio is attributable to an identifiable build.
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

/// Bootstrap 95% CI for the MEDIAN. Fixed seed, so the same ELF on the same
/// samples reproduces the interval exactly.
fn bootstrap_median_ci(samples: &[f64], seed: u64) -> (f64, f64) {
    const RESAMPLES: usize = 10_000;
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

/// A runtime holding `keys` string values, with latency tracking set as asked.
fn runtime_with_keys(keys: &[Vec<u8>], latency_tracking: bool) -> Runtime {
    let mut rt = Runtime::new(RuntimePolicy::default());
    rt.set_latency_tracking(latency_tracking);
    for key in keys {
        rt.execute_shared_nothing_set(key, b"value-payload", 1);
    }
    rt
}

fn main() {
    match bench_elf_sha256() {
        Ok((sha256, bytes)) => println!("bench_elf_sha256={sha256} ({bytes} bytes)"),
        Err(error) => panic!("ELF SELF-REPORT INVALID: {error}"),
    }

    // Correctness gate before any timing: recording must not change the REPLY,
    // only the bookkeeping. If the two arms disagreed on bytes, the ratio would
    // be comparing two different behaviours.
    {
        let keys: Vec<Vec<u8>> = (0..8).map(|i| format!("gate:{i}").into_bytes()).collect();
        let mut tracked = runtime_with_keys(&keys, true);
        let mut untracked = runtime_with_keys(&keys, false);
        for key in &keys {
            let mut a = Vec::new();
            let mut b = Vec::new();
            tracked.execute_shared_nothing_get_into(key, 1, &mut a);
            untracked.execute_shared_nothing_get_into(key, 1, &mut b);
            assert_eq!(a, b, "latency tracking must not change the reply bytes");
        }
        let mut miss_a = Vec::new();
        let mut miss_b = Vec::new();
        tracked.execute_shared_nothing_get_into(b"absent", 1, &mut miss_a);
        untracked.execute_shared_nothing_get_into(b"absent", 1, &mut miss_b);
        assert_eq!(
            miss_a, miss_b,
            "a miss must render identically in both arms"
        );
    }

    let cases: &[(&str, usize)] = &[("keys_16", 16), ("keys_256", 256), ("keys_4096", 4096)];

    println!(
        "\n{:<12} {:>8} {:>10} {:>18} {:>8} {:>14} {:>20}",
        "workload", "reps", "NULL med", "null median CI95", "null cv%", "on/off", "verdict"
    );

    for (label, key_count) in cases {
        let keys: Vec<Vec<u8>> = (0..*key_count)
            .map(|i| format!("bench:key:{i}").into_bytes())
            .collect();
        let mut tracked = runtime_with_keys(&keys, true);
        let mut untracked_a = runtime_with_keys(&keys, false);
        let mut untracked_b = runtime_with_keys(&keys, false);

        let mut scratch = Vec::with_capacity(64);
        let mut time = |rt: &mut Runtime, reps: usize| -> f64 {
            let start = Instant::now();
            let mut sink = 0usize;
            for _ in 0..reps {
                for key in &keys {
                    scratch.clear();
                    rt.execute_shared_nothing_get_into(black_box(key), 1, &mut scratch);
                    // Fold the reply so the encode cannot be elided.
                    sink = sink.wrapping_add(scratch.iter().map(|b| *b as usize).sum::<usize>());
                }
            }
            black_box(sink);
            start.elapsed().as_secs_f64()
        };

        let mut reps = 1usize;
        loop {
            let elapsed = time(&mut untracked_a, reps);
            if elapsed >= TARGET_SEGMENT_SECS || reps > 1 << 16 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / elapsed.max(1e-9)).max(1.0)).ceil()
                    as usize;
                break;
            }
            reps *= 4;
        }

        let mut order_state: u64 = 0x2545F4914F6CDD1D ^ (reps as u64);
        let mut next_swap = move || {
            order_state ^= order_state << 13;
            order_state ^= order_state >> 7;
            order_state ^= order_state << 17;
            order_state & 1 == 1
        };

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut ratios = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            // A/A: two runtimes that are configured identically.
            let null = if next_swap() {
                let second = time(&mut untracked_b, reps);
                time(&mut untracked_a, reps) / second
            } else {
                let first = time(&mut untracked_a, reps);
                first / time(&mut untracked_b, reps)
            };
            // A/B: tracking on over tracking off. >1 means recording costs time.
            let ratio = if next_swap() {
                let on = time(&mut tracked, reps);
                on / time(&mut untracked_a, reps)
            } else {
                let off = time(&mut untracked_a, reps);
                time(&mut tracked, reps) / off
            };
            if round == 0 {
                continue;
            }
            nulls.push(null);
            ratios.push(ratio);
        }

        let null_med = median(&mut nulls.clone());
        let (null_lo, null_hi) = bootstrap_median_ci(&nulls, 0x9E3779B97F4A7C15);
        let admissible = null_lo <= 1.0 && null_hi >= 1.0;
        let ratio_med = median(&mut ratios.clone());
        let (ratio_lo, ratio_hi) = bootstrap_median_ci(&ratios, 0xD1B54A32D192ED03);
        let verdict = if !admissible {
            "NULL-INADMISSIBLE".to_string()
        } else if ratio_lo > 1.0 && ratio_lo > null_hi {
            format!("COSTS [{ratio_lo:.3},{ratio_hi:.3}]")
        } else if ratio_hi < 1.0 && ratio_hi < null_lo {
            "FASTER(?)".to_string()
        } else {
            "indistinguishable".to_string()
        };

        println!(
            "{label:<12} {reps:>8} {null_med:>10.4} {:>18} {:>8.2} {ratio_med:>13.3}x {verdict:>20}",
            format!("[{null_lo:.4}, {null_hi:.4}]"),
            cv(&nulls),
        );
    }
}
