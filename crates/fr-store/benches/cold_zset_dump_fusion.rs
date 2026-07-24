/*
//! Same-binary A/A+A/B for the cold compact-ZSET DUMP score-classifier trunc elision
//! (`frankenredis-hdyw0`).
//!
//! Every call removes only the benchmark key's cached payload, then executes the real DUMP path.
//! The baseline retains `f64::fract()` while the candidate classifies integral values from their
//! IEEE-754 exponent/fraction bits. Both arms include lookup/touch, listpack encoding, LZF, CRC,
//! cache insertion, and the returned payload clone. They are interleaved within each round with a
//! position-balanced A/A null control; inputs and results cross optimizer barriers.

use std::hint::black_box;
use std::process::Command;

use fr_store::Store;

const KEY: &[u8] = b"z";
const NOW_MS: u64 = 7;
const PROFILE_PASSES: usize = 160_000;
const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.025;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn build(fractional: bool) -> Store {
    let mut store = Store::new();
    let pairs = (0..128)
        .map(|index| {
            let score = if !fractional || index % 3 == 0 {
                index as f64 - 64.0
            } else {
                index as f64 * 1.25 + 0.125
            };
            (score, format!("member:{index:03}").into_bytes())
        })
        .collect::<Vec<_>>();
    store.zadd(KEY, &pairs, NOW_MS).expect("seed compact zset");
    store
}

#[inline(never)]
fn run_fract(store: &mut Store, passes: usize) -> u64 {
    let mut checksum = 0_u64;
    for _ in 0..passes {
        let payload = store
            .dump_key_cold_fract_bench(black_box(KEY), black_box(NOW_MS))
            .expect("cold zset DUMP");
        checksum = checksum
            .wrapping_add(payload.len() as u64)
            .wrapping_add(u64::from(payload[0]));
        black_box(payload);
    }
    black_box(checksum)
}

#[inline(never)]
fn run_bitwise(store: &mut Store, passes: usize) -> u64 {
    let mut checksum = 0_u64;
    for _ in 0..passes {
        let payload = store
            .dump_key_cold_bitwise_bench(black_box(KEY), black_box(NOW_MS))
            .expect("cold zset DUMP");
        checksum = checksum
            .wrapping_add(payload.len() as u64)
            .wrapping_add(u64::from(payload[0]));
        black_box(payload);
    }
    black_box(checksum)
}

fn profile_child() {
    let mut store = build(true);
    black_box(run_fract(&mut store, PROFILE_PASSES));
}

fn run_profile_if_requested() -> bool {
    if std::env::var_os("COLD_ZSET_DUMP_PROFILE_CHILD").is_some() {
        profile_child();
        return true;
    }
    if std::env::var_os("COLD_ZSET_DUMP_PROFILE").is_none() {
        return false;
    }

    let exe = std::env::current_exe().expect("current benchmark executable");
    let data = "/tmp/cold_zset_dump_fusion.perf.data";
    let status = Command::new("perf")
        .args([
            "record", "-q", "-e", "cycles:u", "-F", "999", "-o", data, "--",
        ])
        .arg(exe)
        .env("COLD_ZSET_DUMP_PROFILE_CHILD", "1")
        .status()
        .expect("run perf record");
    assert!(status.success(), "perf record failed: {status}");

    let report = Command::new("perf")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--sort=symbol",
            "--percent-limit=0.1",
            "-i",
            data,
        ])
        .output()
        .expect("run perf report");
    assert!(
        report.status.success(),
        "perf report failed: {}",
        report.status
    );
    print!("{}", String::from_utf8_lossy(&report.stdout));
    true
}

type Arm = fn(&mut Store, usize) -> u64;

fn time(store: &mut Store, arm: Arm, reps: usize) -> f64 {
    let start = std::time::Instant::now();
    black_box(arm(black_box(store), black_box(reps)));
    start.elapsed().as_secs_f64()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|left, right| left.partial_cmp(right).expect("finite ratio"));
    values[values.len() / 2]
}

fn cv(values: &[f64]) -> f64 {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    100.0
        * (values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / values.len() as f64)
            .sqrt()
        / mean
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn print_provenance() {
    let exe = std::env::current_exe().expect("current benchmark executable");
    let output = Command::new("sha256sum")
        .arg(&exe)
        .output()
        .expect("run sha256sum");
    assert!(output.status.success(), "sha256sum failed");
    print!("binary {}", String::from_utf8_lossy(&output.stdout));
}

fn bench_case(label: &str, fractional: bool) {
    let mut store = build(fractional);
    let reference = store
        .dump_key_cold_fract_bench(KEY, NOW_MS)
        .expect("reference DUMP");
    let candidate = store
        .dump_key_cold_bitwise_bench(KEY, NOW_MS)
        .expect("candidate DUMP");
    assert_eq!(reference, candidate, "{label}: DUMP bytes diverged");

    let mut reps = 1_usize;
    loop {
        let elapsed = time(&mut store, run_fract, reps);
        if elapsed >= TARGET_SEGMENT_SECS || reps > 1 << 20 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / elapsed.max(1e-9)).max(1.0)).ceil()
                as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speedups = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let mut pair = |baseline: Arm, candidate: Arm| {
            if swap {
                let candidate_secs = time(&mut store, candidate, reps);
                time(&mut store, baseline, reps) / candidate_secs
            } else {
                let baseline_secs = time(&mut store, baseline, reps);
                baseline_secs / time(&mut store, candidate, reps)
            }
        };
        let null = pair(run_fract, run_fract);
        let speedup = pair(run_fract, run_bitwise);
        if round == 0 {
            continue;
        }
        nulls.push(null);
        speedups.push(speedup);
    }

    let null_median = median(&mut nulls);
    let speedup = median(&mut speedups);
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
        "{label:<12} {reps:>7} {null_median:>9.4} {:>16} {:>8.2} {speedup:>12.4}x {verdict:>16}",
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
    );
}

fn main() {
    if run_profile_if_requested() {
        return;
    }
    print_provenance();
    println!(
        "\n{:<12} {:>7} {:>9} {:>16} {:>8} {:>13} {:>16}",
        "scores", "reps", "NULL med", "null p5..p95", "null cv%", "bitwise/fract", "verdict"
    );
    bench_case("mixed128", true);
    bench_case("integer128", false);
}
*/
