//! Instruction-count microbench for `PackedZSet::locate` (behind ZSCORE/ZADD/ZREM/ZRANK on a
//! packed/listpack zset). A ZSCORE of a MISSING member is the full-scan worst case: `locate` walks
//! every record. The pre-cc scan decoded each record's 8-byte f64 score (a bounds-checked load) via
//! `record_at`, even though only the matched record's score is ever used; the new `locate` skips the
//! score for non-matching records. Measured before/after (via `git stash` of packed_set.rs) with
//! `perf stat -e instructions:u` — the eliminated per-record bounds-check+load is deterministic and
//! survives DCE (the score slice can panic), so wallclock noise doesn't obscure it.
//!
//! FR_ITERS controls the loop count (default 20M). The zset holds 120 short members so it stays
//! listpack-encoded (< zset-max-listpack-entries 128) — the PackedZSet path.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

fn main() {
    let mut s = Store::new();
    let members: Vec<(f64, Vec<u8>)> = (0..120u32)
        .map(|i| (f64::from(i), format!("member:{i:04}").into_bytes()))
        .collect();
    s.zadd(b"z", &members, 1).unwrap();

    let missing: &[u8] = b"member:zzzz"; // absent → locate full-scans all 120 records
    let iters: u64 = std::env::var("FR_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000_000);

    let start = Instant::now();
    let mut acc = 0u64;
    for _ in 0..iters {
        if s.zscore(black_box(b"z"), black_box(missing), 1)
            .unwrap()
            .is_some()
        {
            acc += 1;
        }
    }
    black_box(acc);
    let secs = start.elapsed().as_secs_f64();
    println!(
        "packed_zset_locate: {iters} zscore-miss on 120-member packed zset in {secs:.3}s ({:.2} ns/op)",
        secs * 1e9 / iters as f64
    );
}
