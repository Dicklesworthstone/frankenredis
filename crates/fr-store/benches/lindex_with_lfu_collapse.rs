//! Same-binary A/A+A/B for the LFU acquisition in zero-copy `lindex_with`. The hot runtime
//! sequence performs the unchanged `key_type` precheck, then the prior LINDEX path probes once to
//! gate its LFU draw and again to resolve the list entry. Production folds those latter two probes
//! into one `get_mut`; both arms retain the borrowed-result callback and list indexing.

use std::hint::black_box;
use std::process::Command;

use fr_store::{MaxmemoryPolicy, Store};

const KEYSPACE: usize = 20_000;
const PASSES: usize = 16;
const PROFILE_PASSES: usize = 128;
const ROUNDS: usize = 9;
const INDEX: i64 = 1;

fn build() -> (Store, Vec<Vec<u8>>) {
    let mut store = Store::new();
    let mut keys = Vec::with_capacity(KEYSPACE);
    for i in 0..KEYSPACE {
        let key = format!("k{i:08}").into_bytes();
        store
            .rpush(
                &key,
                &[b"a".to_vec(), b"value123".to_vec(), b"z".to_vec()],
                1,
            )
            .unwrap();
        keys.push(key);
    }
    store.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    store.lfu_decay_time = 0;
    (store, keys)
}

fn consume(result: Result<u64, fr_store::StoreError>) -> u64 {
    black_box(result).unwrap_or(0xd1b5_4a32_d192_ed03)
}

#[inline(never)]
fn run_twoprobe(store: &mut Store, keys: &[Vec<u8>]) -> u64 {
    let mut acc = 0_u64;
    for key in keys {
        let key = black_box(key.as_slice());
        let result = match black_box(store.key_type(key, black_box(1))) {
            Some("list") => {
                store.lindex_with_lfu_twoprobe_bench(key, black_box(INDEX), black_box(1), |value| {
                    black_box(value).map_or(0, |bytes| bytes.len() as u64)
                })
            }
            Some(_) => Ok(0xa076_1d64_78bd_642f),
            None => Ok(0),
        };
        acc = acc.wrapping_add(consume(result));
    }
    black_box(acc)
}

#[inline(never)]
fn run_collapsed(store: &mut Store, keys: &[Vec<u8>]) -> u64 {
    let mut acc = 0_u64;
    for key in keys {
        let key = black_box(key.as_slice());
        let result = match black_box(store.key_type(key, black_box(1))) {
            Some("list") => store.lindex_with(key, black_box(INDEX), black_box(1), |value| {
                black_box(value).map_or(0, |bytes| bytes.len() as u64)
            }),
            Some(_) => Ok(0xa076_1d64_78bd_642f),
            None => Ok(0),
        };
        acc = acc.wrapping_add(consume(result));
    }
    black_box(acc)
}

fn workload(collapsed: bool) {
    let (mut store, keys) = build();
    let mut acc = 0_u64;
    for _ in 0..PASSES {
        acc = acc.wrapping_add(if collapsed {
            run_collapsed(black_box(&mut store), black_box(&keys))
        } else {
            run_twoprobe(black_box(&mut store), black_box(&keys))
        });
    }
    black_box(acc);
}

fn profile_child() {
    let (mut prior, keys) = build();
    let (mut collapsed, _) = build();
    let mut acc = 0_u64;
    for _ in 0..PROFILE_PASSES {
        acc = acc.wrapping_add(run_twoprobe(black_box(&mut prior), black_box(&keys)));
        acc = acc.wrapping_add(run_collapsed(black_box(&mut collapsed), black_box(&keys)));
    }
    black_box(acc);
}

fn profile_exact_input() {
    let exe = std::env::current_exe().expect("current benchmark executable");
    let data = "/tmp/lindex_with_lfu_collapse.perf.data";
    let status = Command::new("perf")
        .args([
            "record", "-q", "-e", "cycles:u", "-F", "999", "-o", data, "--",
        ])
        .arg(&exe)
        .env("LINDEX_PROFILE_CHILD", "1")
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
    println!(
        "exact-input profile:\n{}",
        String::from_utf8_lossy(&report.stdout)
    );
}

fn perf_count(mode: &str) -> u64 {
    let exe = std::env::current_exe().expect("current benchmark executable");
    let output = Command::new("perf")
        .args(["stat", "-x", ",", "-e", "instructions:u", "--"])
        .arg(&exe)
        .env("LINDEX_PERF_MODE", mode)
        .output()
        .expect("run perf stat");
    assert!(
        output.status.success(),
        "perf stat failed: {}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .find(|line| line.contains("instructions:u"))
        .and_then(|line| line.split(',').next())
        .map(str::trim)
        .map(|field| field.replace(' ', ""))
        .and_then(|field| field.parse::<u64>().ok())
        .expect("parse instructions:u count")
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    values[values.len() / 2]
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
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

fn main() {
    if std::env::var_os("LINDEX_PROFILE_CHILD").is_some() {
        profile_child();
        return;
    }
    if let Some(mode) = std::env::var_os("LINDEX_PERF_MODE") {
        match mode.to_string_lossy().as_ref() {
            "build" => {
                let built = build();
                black_box(built);
            }
            "base" => workload(false),
            "coll" => workload(true),
            _ => panic!("unknown LINDEX_PERF_MODE"),
        }
        return;
    }

    print_provenance();
    profile_exact_input();

    let mut null_ratios = Vec::with_capacity(ROUNDS);
    let mut candidate_ratios = Vec::with_capacity(ROUNDS);
    let mut baseline_per_op = Vec::with_capacity(ROUNDS);
    let mut collapsed_per_op = Vec::with_capacity(ROUNDS);
    let operations = (KEYSPACE * PASSES) as f64;
    for round in 0..ROUNDS {
        let build_count = perf_count("build");
        let order = match round % 6 {
            0 => [("a", "base"), ("b", "base"), ("c", "coll")],
            1 => [("c", "coll"), ("b", "base"), ("a", "base")],
            2 => [("b", "base"), ("c", "coll"), ("a", "base")],
            3 => [("a", "base"), ("c", "coll"), ("b", "base")],
            4 => [("b", "base"), ("a", "base"), ("c", "coll")],
            _ => [("c", "coll"), ("a", "base"), ("b", "base")],
        };
        let (mut a, mut b, mut candidate) = (0_u64, 0_u64, 0_u64);
        for (label, mode) in order {
            let count = perf_count(mode).saturating_sub(build_count);
            match label {
                "a" => a = count,
                "b" => b = count,
                "c" => candidate = count,
                _ => unreachable!(),
            }
        }
        let baseline = (a as f64 + b as f64) / 2.0;
        null_ratios.push(a as f64 / b.max(1) as f64);
        candidate_ratios.push(baseline / candidate.max(1) as f64);
        baseline_per_op.push(baseline / operations);
        collapsed_per_op.push(candidate as f64 / operations);
    }

    let null_cv = cv(&null_ratios);
    let candidate_cv = cv(&candidate_ratios);
    let null_median = median(&mut null_ratios);
    let null_lo = percentile(&null_ratios, 0.05);
    let null_hi = percentile(&null_ratios, 0.95);
    let candidate_median = median(&mut candidate_ratios);
    let baseline_median = median(&mut baseline_per_op);
    let collapsed_median = median(&mut collapsed_per_op);
    let verdict = if candidate_median > 1.0 && candidate_median > null_hi {
        "WIN"
    } else if candidate_median < 1.0 && candidate_median < null_lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!("LINDEX zero-copy LFU 2->1 after key_type (instructions:u, {ROUNDS} balanced rounds)");
    println!(
        "  A/A null       : median {null_median:.4}  p5..p95 [{null_lo:.4}, {null_hi:.4}]  cv {null_cv:.2}%"
    );
    println!(
        "  baseline       : {baseline_median:.2} instr/op\n  collapsed      : {collapsed_median:.2} instr/op"
    );
    println!("  baseline/coll  : {candidate_median:.4}x  cv {candidate_cv:.2}%  {verdict}");
}
