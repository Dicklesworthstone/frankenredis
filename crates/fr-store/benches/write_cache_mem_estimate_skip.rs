//! Same-binary A/B for eliminating the redundant per-WRITE `mem_estimate_cache` remove.
//!
//! Every scalar write (SET/INCR insert, in-place INCR, SET KEEPTTL) invalidates the write-side
//! caches. The `mem_estimate_cache` is MOD-COUNT-VERSIONED — its only reader,
//! `cached_entry_memory_usage_bytes`, recomputes on a mod-count mismatch — so a write need NOT
//! remove its entry; a stale entry is recomputed on the next read. But the pre-change invalidation
//! removed it unconditionally-when-non-empty, and `HashMap::remove` HASHES the key even on a miss.
//! Whenever the cache holds ANY entry (a restored/large collection makes `!is_empty()` pass), every
//! scalar-write key therefore paid a doomed foldhash-and-probe — measured at ~11% of INCR self-time
//! in a live perf-record. The delete path still purges the entry (`invalidate_side_caches_on_delete`).
//!
//! Reference = the delete-path invalidation (`invalidate_write_side_caches_new`, removes mem);
//! candidate = the write-path invalidation (`invalidate_write_side_caches_write_path`, skips mem).
//! The cache is seeded non-empty (as a live server with collections is) and the target key is a
//! MISS, so the only per-call difference is the eliminated mem-cache remove-miss. Both arms leave
//! the cache unchanged (miss), so state is stable across reps. Byte-identical for all observable
//! MEMORY USAGE results (mod-count recompute), asserted by the fr-store unit tests.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"counter:key:00000042";
const CACHE_ENTRIES: usize = 256;

fn seeded_store() -> Store {
    let mut s = Store::new();
    s.bench_seed_mem_estimate_cache(CACHE_ENTRIES);
    s
}

/// Reference: delete-path invalidation — removes the (absent) key from the non-empty
/// mem_estimate_cache, hashing it against a doomed remove-miss.
fn removes_mem(s: &mut Store) {
    s.invalidate_write_side_caches_new(black_box(KEY));
}
/// Candidate: scalar-write invalidation (INCR path) — skips the mem_estimate_cache remove (the
/// key, an integer, is never a member of that cache, so the remove was an unconditional miss).
fn skips_mem(s: &mut Store) {
    s.invalidate_write_side_caches_scalar_shim(black_box(KEY));
}

fn timed(f: fn(&mut Store), s: &mut Store, reps: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..reps {
        f(s);
    }
    black_box(&*s);
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
    // Correctness: KEY (an integer counter) is never a cache member, so both the scalar-skip and
    // the full invalidation are a no-op miss — the seeded cache is left identical by both arms.
    {
        let mut a = seeded_store();
        let before = a.bench_mem_estimate_cache_len();
        a.invalidate_write_side_caches_scalar_shim(KEY);
        assert_eq!(a.bench_mem_estimate_cache_len(), before, "scalar skip must not touch a miss");
        a.invalidate_write_side_caches_new(KEY);
        assert_eq!(a.bench_mem_estimate_cache_len(), before, "full invalidation miss also no-op");
    }

    let mut store_o = seeded_store();
    let mut store_n = seeded_store();

    let mut reps = 1usize;
    loop {
        let e = timed(removes_mem, &mut store_o, reps);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 22 {
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
            let c = timed(removes_mem, &mut store_o, reps);
            timed(removes_mem, &mut store_o, reps) / c
        } else {
            let b = timed(removes_mem, &mut store_o, reps);
            b / timed(removes_mem, &mut store_o, reps)
        };
        let sp = if swap {
            let c = timed(skips_mem, &mut store_n, reps);
            timed(removes_mem, &mut store_o, reps) / c
        } else {
            let b = timed(removes_mem, &mut store_o, reps);
            b / timed(skips_mem, &mut store_n, reps)
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
        "\n{:<28} {:>8} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    println!(
        "{:<28} {:>8} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
        "write_side_mem_skip",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
