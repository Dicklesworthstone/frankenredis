//! Same-binary A/B for the LFU ZREVRANK keyspace-probe collapse. Under allkeys-lfu the prior
//! ZREVRANK did three `entries` probes: `record_keyspace_lookup` + a `contains_key` random-sample
//! gate + `get_mut`. The candidate folds them into one `get_mut`, with inline hit accounting and an
//! exact field-split RNG draw. Singleton packed-zset reverse-rank lookup keeps probe cost visible.
//!
//! Both arms live in this binary, execute interleaved within each measured round, and consume the
//! exact same present-key/present-member workload. The A/A control supplies the per-size null band.

use std::hint::black_box;
use std::process::Command;
use std::time::Instant;

use fr_store::{MaxmemoryPolicy, Store};

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.04;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEYSPACE: usize = 50_000;
const MEMBER: &[u8] = b"m";

fn build() -> Store {
    let mut store = Store::new();
    store.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    store.lfu_decay_time = 0;
    for i in 0..KEYSPACE {
        let key = format!("k{i:08}").into_bytes();
        store.zadd(&key, &[(1.25, MEMBER.to_vec())], 1).unwrap();
    }
    store
}

fn zrevrank_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("k{:08}", i * (KEYSPACE / n.max(1))).into_bytes())
        .collect()
}

#[inline(never)]
fn run_threeprobe(store: &mut Store, keys: &[&[u8]], member: &[u8]) -> u64 {
    let mut acc = 0u64;
    for &key in keys {
        let result =
            store.zrevrank_lfu_threeprobe_bench(black_box(key), black_box(member), black_box(1));
        acc = acc.wrapping_add(match black_box(result) {
            Ok(Some(rank)) => rank as u64,
            Ok(None) => 0x9e37_79b9_7f4a_7c15,
            Err(_) => 0xd1b5_4a32_d192_ed03,
        });
    }
    black_box(acc)
}

#[inline(never)]
fn run_collapse(store: &mut Store, keys: &[&[u8]], member: &[u8]) -> u64 {
    let mut acc = 0u64;
    for &key in keys {
        let result =
            store.zrevrank_lfu_collapsed_bench(black_box(key), black_box(member), black_box(1));
        acc = acc.wrapping_add(match black_box(result) {
            Ok(Some(rank)) => rank as u64,
            Ok(None) => 0x9e37_79b9_7f4a_7c15,
            Err(_) => 0xd1b5_4a32_d192_ed03,
        });
    }
    black_box(acc)
}

type Arm = fn(&mut Store, &[&[u8]], &[u8]) -> u64;

fn time(reps: usize, store: &mut Store, arm: Arm, keys: &[&[u8]], member: &[u8]) -> f64 {
    let start = Instant::now();
    let mut acc = 0u64;
    for _ in 0..reps {
        acc = acc.wrapping_add(arm(black_box(store), black_box(keys), black_box(member)));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
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

fn profile_child() {
    let mut store = build();
    let owned = zrevrank_keys(256);
    let keys: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let mut acc = 0u64;
    for _ in 0..65_536 {
        acc = acc.wrapping_add(run_threeprobe(
            black_box(&mut store),
            black_box(&keys),
            black_box(MEMBER),
        ));
        acc = acc.wrapping_add(run_collapse(
            black_box(&mut store),
            black_box(&keys),
            black_box(MEMBER),
        ));
    }
    black_box(acc);
}

fn run_profile_if_requested() -> bool {
    if std::env::var_os("ZREVRANK_PROFILE_CHILD").is_some() {
        profile_child();
        return true;
    }
    if std::env::var_os("ZREVRANK_PROFILE").is_none() {
        return false;
    }

    let exe = std::env::current_exe().expect("current benchmark executable");
    let data = "/tmp/zrevrank_lfu_collapse.perf.data";
    let status = Command::new("perf")
        .args([
            "record", "-q", "-e", "cycles:u", "-F", "999", "-o", data, "--",
        ])
        .arg(exe)
        .env("ZREVRANK_PROFILE_CHILD", "1")
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

fn print_provenance() {
    let exe = std::env::current_exe().expect("current benchmark executable");
    let output = Command::new("sha256sum")
        .arg(&exe)
        .output()
        .expect("run sha256sum");
    assert!(
        output.status.success(),
        "sha256sum failed: {}",
        output.status
    );
    print!("binary {}", String::from_utf8_lossy(&output.stdout));
}

fn bench(label: &str, store: &mut Store, n: usize) {
    let owned = zrevrank_keys(n);
    let keys: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();

    let mut reps = 1usize;
    loop {
        let elapsed = time(reps, store, run_threeprobe, &keys, MEMBER);
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
                let candidate_secs = time(reps, store, candidate, &keys, MEMBER);
                time(reps, store, baseline, &keys, MEMBER) / candidate_secs
            } else {
                let baseline_secs = time(reps, store, baseline, &keys, MEMBER);
                baseline_secs / time(reps, store, candidate, &keys, MEMBER)
            }
        };
        let null = pair(run_threeprobe, run_threeprobe);
        let speedup = pair(run_threeprobe, run_collapse);
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
        "{label:<10} {reps:>7} {null_median:>9.4} {:>16} {:>8.2} {speedup:>10.3}x {verdict:>16}",
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
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_zrevrank", "reps", "NULL med", "null p5..p95", "null cv%", "collapse/3p", "verdict"
    );
    let mut store = build();
    bench("n32", &mut store, 32);
    bench("n256", &mut store, 256);
}
