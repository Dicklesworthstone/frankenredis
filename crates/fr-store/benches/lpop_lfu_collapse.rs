//! instructions:u A/B for the LFU LPOP write-side keyspace-probe collapse — the prior path gated the
//! LFU rand behind a separate `self.entries.contains_key(key)` probe BEFORE `get_mut`; production draws
//! it on the disjoint `&mut self.rng_seed` field split INSIDE the single `get_mut` `Some` arm (2 probes
//! -> 1). Byte/RNG-identical (`lpop_lfu_collapsed_matches_twoprobe`).
//!
//! LPOP GROWS the list, so a wallclock criterion loop isn't repeatable (the list balloons + switches
//! encoding). Instead this counts retired user-space instructions via `perf stat -e instructions:u`,
//! which is deterministic (so even a small probe elision is cleanly measurable, not noise-limited).
//! Three self-invoked child modes let the parent subtract the IDENTICAL build cost and isolate the
//! per-op workload: `build` (seed 50k single-element lists), `base` (build + N LPOPes via the
//! two-probe baseline), `coll` (build + N LPOPes via production). The lists are pre-seeded so every
//! timed LPOP hits the existing-key APPEND arm (the collapse target), not the create arm.

use std::hint::black_box;
use std::process::Command;

use fr_store::{MaxmemoryPolicy, Store};

const KEYSPACE: usize = 50_000;
const PASSES: usize = 40; // KEYSPACE*PASSES = 2,000,000 timed LPOPes
const RUNS: usize = 5;

fn build() -> Store {
    let mut s = Store::new();
    s.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    s.lfu_decay_time = 0;
    // Seed each list with more than PASSES elements so the timed LPOPs never empty a key — every
    // timed op hits the existing-list `Some` arm (the collapse target), not the absent `None` arm.
    let seed: Vec<Vec<u8>> = (0..(PASSES + 8)).map(|_| b"s".to_vec()).collect();
    for i in 0..KEYSPACE {
        let k = format!("k{i:08}").into_bytes();
        s.rpush(&k, &seed, 1).unwrap();
    }
    s
}

fn keys() -> Vec<Vec<u8>> {
    (0..KEYSPACE)
        .map(|i| format!("k{i:08}").into_bytes())
        .collect()
}

fn workload(collapsed: bool) {
    let mut s = build();
    let ks = keys();
    let mut acc = 0usize;
    for _ in 0..PASSES {
        for k in &ks {
            let r = if collapsed {
                s.lpop(black_box(k.as_slice()), 1)
            } else {
                s.lpop_lfu_twoprobe_bench(black_box(k.as_slice()), 1)
            };
            acc = acc.wrapping_add(r.ok().flatten().map_or(0, |v| v.len()));
        }
    }
    black_box(acc);
}

fn perf_count(mode: &str) -> Option<u64> {
    let exe = std::env::current_exe().ok()?;
    let out = Command::new("perf")
        .args(["stat", "-x", ",", "-e", "instructions:u", "--"])
        .arg(&exe)
        .env("LPOP_PERF_MODE", mode)
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
        if line.contains("instructions:u") {
            let field = line.split(',').next().unwrap_or("").trim().replace(' ', "");
            if let Ok(n) = field.parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    if let Ok(mode) = std::env::var("LPOP_PERF_MODE") {
        match mode.as_str() {
            "build" => {
                let s = build();
                black_box(&s);
            }
            "base" => workload(false),
            "coll" => workload(true),
            _ => {}
        }
        return;
    }

    // Warm one child so page-ins / first-run effects don't skew the first sample.
    let _ = perf_count("build");
    let (mut base_w, mut coll_w) = (Vec::new(), Vec::new());
    for _ in 0..RUNS {
        let (b, base, coll) = match (perf_count("build"), perf_count("base"), perf_count("coll")) {
            (Some(b), Some(base), Some(coll)) => (b, base, coll),
            _ => {
                eprintln!(
                    "perf stat -e instructions:u unavailable (perf_event_paranoid / no perf) — cannot measure"
                );
                return;
            }
        };
        base_w.push(base.saturating_sub(b));
        coll_w.push(coll.saturating_sub(b));
    }
    let base_med = median(base_w);
    let coll_med = median(coll_w);
    let ops = (KEYSPACE * PASSES) as f64;
    println!(
        "\nLPOP LFU 2->1 (instructions:u, {} timed appends, build subtracted, median of {RUNS})",
        KEYSPACE * PASSES
    );
    println!("  baseline workload : {base_med:>14} instr  ({:.2}/op)", base_med as f64 / ops);
    println!("  collapsed workload: {coll_med:>14} instr  ({:.2}/op)", coll_med as f64 / ops);
    println!(
        "  saved             : {:>14} instr  ({:.2}/op)",
        base_med.saturating_sub(coll_med),
        (base_med as f64 - coll_med as f64) / ops
    );
    let ratio = base_med as f64 / coll_med.max(1) as f64;
    println!(
        "  ratio base/coll   : {ratio:.4}x  ({})",
        if coll_med < base_med { "WIN (fewer retired instructions)" } else { "no reduction" }
    );
}
