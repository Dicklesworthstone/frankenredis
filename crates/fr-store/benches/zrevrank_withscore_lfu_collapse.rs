//! Same-binary A/A+A/B for the LFU `ZREVRANK ... WITHSCORE` acquisition collapse. The prior route
//! performs `record_keyspace_lookup` + a `contains_key` random-sample gate + `get_mut`; the candidate
//! folds them into one `get_mut`. A singleton packed zset keeps the removed keyspace probes visible.
//!
//! The executable first profiles the exact two timed wrappers with `perf record`, then runs a
//! position-balanced interleaved wall-clock sweep. Inputs and results cross `black_box` barriers.

use std::hint::black_box;
use std::process::Command;
use std::time::Instant;

use fr_store::{MaxmemoryPolicy, Store};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.02;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEYSPACE: usize = 8_192;
const PROFILE_ROUNDS: usize = 16_384;
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

fn keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("k{:08}", i * (KEYSPACE / n.max(1))).into_bytes())
        .collect()
}

#[inline(never)]
fn run_threeprobe(store: &mut Store, keys: &[&[u8]], member: &[u8]) -> u64 {
    let mut acc = 0_u64;
    for &key in keys {
        let result = store.zrevrank_withscore_lfu_threeprobe_bench(
            black_box(key),
            black_box(member),
            black_box(1),
        );
        acc = acc.wrapping_add(match black_box(result) {
            Ok(Some((rank, score))) => (rank as u64).wrapping_add(score.to_bits()),
            Ok(None) => 0x9e37_79b9_7f4a_7c15,
            Err(_) => 0xd1b5_4a32_d192_ed03,
        });
    }
    black_box(acc)
}

#[inline(never)]
fn run_collapse(store: &mut Store, keys: &[&[u8]], member: &[u8]) -> u64 {
    let mut acc = 0_u64;
    for &key in keys {
        let result = store.zrevrank_withscore_lfu_collapsed_bench(
            black_box(key),
            black_box(member),
            black_box(1),
        );
        acc = acc.wrapping_add(match black_box(result) {
            Ok(Some((rank, score))) => (rank as u64).wrapping_add(score.to_bits()),
            Ok(None) => 0x9e37_79b9_7f4a_7c15,
            Err(_) => 0xd1b5_4a32_d192_ed03,
        });
    }
    black_box(acc)
}

type Arm = fn(&mut Store, &[&[u8]], &[u8]) -> u64;

fn time(reps: usize, store: &mut Store, arm: Arm, keys: &[&[u8]], member: &[u8]) -> f64 {
    let start = Instant::now();
    let mut acc = 0_u64;
    for _ in 0..reps {
        acc = acc.wrapping_add(arm(black_box(store), black_box(keys), black_box(member)));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|left, right| left.partial_cmp(right).expect("no NaN"));
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

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn profile_child() {
    let mut store = build();
    let owned = keys(256);
    let borrowed: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let mut acc = 0_u64;
    for _ in 0..PROFILE_ROUNDS {
        acc = acc.wrapping_add(run_threeprobe(
            black_box(&mut store),
            black_box(&borrowed),
            black_box(MEMBER),
        ));
        acc = acc.wrapping_add(run_collapse(
            black_box(&mut store),
            black_box(&borrowed),
            black_box(MEMBER),
        ));
    }
    black_box(acc);
}

fn profile_self(report: &str, symbol: &str) -> f64 {
    let line = report
        .lines()
        .find(|line| line.contains(symbol))
        .unwrap_or_else(|| panic!("profile has no {symbol} frame; workload INVALID"));
    let self_pct = line
        .split_whitespace()
        .next()
        .expect("profile row has a self percentage")
        .trim_end_matches('%')
        .parse::<f64>()
        .expect("profile self percentage is numeric");
    assert!(
        self_pct > 0.0,
        "{symbol} has zero self-time; workload INVALID"
    );
    self_pct
}

fn run_profile() {
    let exe = std::env::current_exe().expect("current benchmark executable");
    let data = format!(
        "/tmp/zrevrank_withscore_lfu_collapse_{}.perf.data",
        std::process::id()
    );
    let recorded = Command::new("perf")
        .args([
            "record", "-q", "-e", "cycles:u", "-F", "999", "-o", &data, "--",
        ])
        .arg(&exe)
        .env("ZREVRANK_WITHSCORE_PROFILE_CHILD", "1")
        .output()
        .expect("run perf record");
    assert!(
        recorded.status.success(),
        "perf record failed: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    println!(
        "PROFILE_RECORD_STATS_BEGIN\n{}PROFILE_RECORD_STATS_END",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let report = Command::new("perf")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--sort=symbol",
            "--percent-limit=0.1",
            "-i",
            &data,
        ])
        .output()
        .expect("run perf report");
    assert!(
        report.status.success(),
        "perf report failed: {}",
        report.status
    );
    let text = String::from_utf8_lossy(&report.stdout);
    println!("PROFILE_TABLE_BEGIN\n{text}\nPROFILE_TABLE_END");
    let reference_self = profile_self(&text, "run_threeprobe");
    let candidate_self = profile_self(&text, "run_collapse");
    println!("PROFILE_SELF run_threeprobe={reference_self:.2}% run_collapse={candidate_self:.2}%");
}

fn print_provenance() {
    let exe = std::env::current_exe().expect("current benchmark executable");
    let sha = Command::new("sha256sum")
        .arg(&exe)
        .output()
        .expect("run sha256sum");
    assert!(sha.status.success(), "sha256sum failed: {}", sha.status);
    print!("binary {}", String::from_utf8_lossy(&sha.stdout));
    let hostname = Command::new("hostname").output().expect("run hostname");
    assert!(
        hostname.status.success(),
        "hostname failed: {}",
        hostname.status
    );
    print!("worker {}", String::from_utf8_lossy(&hostname.stdout));
}

fn bench(label: &str, store: &mut Store, n: usize) {
    let owned = keys(n);
    let borrowed: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();

    let mut reps = 1_usize;
    loop {
        let elapsed = time(reps, store, run_threeprobe, &borrowed, MEMBER);
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
        let reverse_order = round % 2 == 1;
        let mut pair = |baseline: Arm, candidate: Arm| {
            if reverse_order {
                let candidate_secs = time(reps, store, candidate, &borrowed, MEMBER);
                time(reps, store, baseline, &borrowed, MEMBER) / candidate_secs
            } else {
                let baseline_secs = time(reps, store, baseline, &borrowed, MEMBER);
                baseline_secs / time(reps, store, candidate, &borrowed, MEMBER)
            }
        };
        let null = pair(run_threeprobe, run_threeprobe);
        let speedup = pair(run_threeprobe, run_collapse);
        if round != 0 {
            nulls.push(null);
            speedups.push(speedup);
        }
    }

    let null_median = median(&mut nulls);
    let speedup = median(&mut speedups);
    let lo = percentile(&nulls, NULL_LO);
    let hi = percentile(&nulls, NULL_HI);
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
    if std::env::var_os("ZREVRANK_WITHSCORE_PROFILE_CHILD").is_some() {
        profile_child();
        return;
    }
    print_provenance();
    run_profile();
    println!(
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_zrev_ws", "reps", "NULL med", "null p5..p95", "null cv%", "collapse/3p", "verdict"
    );
    let mut store = build();
    bench("n32", &mut store, 32);
    bench("n256", &mut store, 256);
}
