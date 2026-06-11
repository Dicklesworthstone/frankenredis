# Pass 129 Kept: Cached GET Read Gate

- Bead: `frankenredis-ohsk5.27`
- Agent: `TealOtter`
- Target: GET/P16/C50 read path after the store reply capsule rejection.
- Commit candidate: cache the default borrowed GET read gate across canonical GET packets in one buffered processing pass, invalidating on generic parsed commands and borrowed write replies.

## Baseline and Profile

- Current build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p129_current cargo build --profile release-perf -p fr-server -p fr-bench`
- Baseline GET/P16/C50/1M: `640.1 ms +/- 32.5 ms`
- Fresh profile workload: GET/P16/C50/3M
- Profile throughput: `1,720,993.72 ops/sec`
- Profile latency: p50 `421us`, p95 `678us`, p99 `866us`, p999 `1208us`, max `16991us`
- Profile rows supporting the trial: `[vdso]` `5.79%` self combined, `Store::get_string_bytes` `1.16%`, `process_buffered_frames` `1.04%`, `Runtime::execute_plain_get_borrowed_into` `1.03%`, `encode_bulk_string_slice` `0.81%`, `plain_borrowed_default_key_read_allows` `0.71%`, and `foldhash hash_one<&[u8]>` `0.56%`.

## Candidate

The server loop now computes `plain_borrowed_default_key_read_gate(ts)` once for a run of canonical borrowed GET packets and calls a runtime GET encoder that accepts that precomputed gate. Any generic parsed command, non-movable frame, or borrowed write fast reply clears the cache before the next command can use it. This removes repeated auth/ACL/pause/default-state checks for pure GET pipelines while preserving fallback on state-changing commands.

## Behavior Proof

- Byte-identical current/candidate TCP transcript: `cdb1611ce3ce2738c8583d8afd19c0fcea4f51d2d57082a6bcc83965ea77751f`
- Transcript length: `834` bytes for current and candidate.
- Transcript covered string hit, integer hit, wrong-type error, expired miss with lazy delete, repeated expired miss, `GET`/`SELECT 1`/`GET` cache invalidation, RESP3 nil via `HELLO 3`, `QUIT`, and `DEBUG DIGEST`.
- Isomorphism note: ordering, tie-breaking, floating-point behavior, RNG behavior, expiry propagation ordering, selected-DB behavior, keyspace hit/miss stats, LRU/LFU semantics, commandstats/slowlog/latency/errorstats behavior, output suppression, and response bytes are preserved.

## Gates

- `cargo test -p fr-runtime plain_get_borrowed_into_cached_gate_matches_uncached -- --nocapture`: passed; `rch` fell back locally, with known pre-existing test-only `unused_mut` warning.
- `cargo test -p fr-server process_buffered_frames_invalidates_cached_get_gate_after_generic_state_change -- --nocapture`: passed via `rch` remote.
- `cargo check -p fr-runtime -p fr-server --all-targets`: passed; same known pre-existing test-only warning.
- `cargo clippy -p fr-runtime -p fr-server --lib --bins -- -D warnings`: passed.
- `git diff --check -- crates/fr-runtime/src/lib.rs crates/fr-server/src/main.rs`: passed.
- `cargo fmt -p fr-runtime -p fr-server -- --check`: still blocked by pre-existing broad rustfmt drift in `fr-runtime`; the new hunks were manually adjusted to rustfmt's requested wrapping.
- Candidate release build passed via `rch`.

## Benchmark Result

Paired GET/P16/C50/1M:

- Current mean: `677.3 ms +/- 24.1 ms`
- Candidate mean: `649.6 ms +/- 21.8 ms`
- Hyperfine summary: candidate `1.04x +/- 0.05` faster.

Reversed GET/P16/C50/1M:

- Candidate mean: `655.9 ms +/- 38.6 ms`
- Current mean: `680.1 ms +/- 16.7 ms`
- Hyperfine summary: candidate `1.04x +/- 0.07` faster.

Longer confirmation GET/P16/C50/3M:

- Current mean: `1.798 s +/- 0.085 s`
- Candidate mean: `1.669 s +/- 0.078 s`
- Hyperfine summary: candidate `1.08x +/- 0.07` faster.

## Decision

Kept. The lever is narrow, byte-stable, and confirmed on the longer workload that rejected pass128. Score is above the keep threshold because the impact is repeatable (`4-8%`), confidence is high after paired/reversed/long confirmation plus byte parity, and implementation effort/risk is low.

Next route: re-profile pushed main after this keep. Do not repeat direct parser shortcuts, store-owned GET reply/read capsules, or the same cached default GET read gate; attack the shifted profile, likely remaining clock/timing, output/writer, or store read surfaces.
