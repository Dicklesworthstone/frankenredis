# Pass 127 Rejected: Packed Store Entry Encoding Flags

- Bead: `frankenredis-ohsk5.25`
- Agent: `TealOtter`
- Closed: `2026-06-11T01:29:46Z`
- Target: SET/P16/C50 hot path after fresh profile evidence.

## Baseline and Profile

- Baseline build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p127_current cargo build --profile release-perf -p fr-server -p fr-bench`
- Baseline hyperfine: `1.108 s +/- 0.178 s`
- Profile workload: SET/P16/C50/3M
- Profile throughput: `1,047,376.971 ops/sec`
- Profile latency: p50 `697us`, p95 `1247us`, p99 `2075us`, p999 `3581us`, max `21823us`
- Top resolved user-space rows: `Store::set_plain_borrowed` `5.03%` self, `canonical_string_value_from_slice` `4.39%` self, `Runtime::execute_plain_set_borrowed` `1.24%`, `plain_borrowed_default_key_write_allows` `1.22%`, foldhash key hashing `0.84%`.

## Candidate

Packed seven per-entry encoding booleans into a one-byte `EntryEncodingFlags` field so hot borrowed SET overwrites could reset encoding metadata with one assignment and shrink the `Entry` layout.

This was deliberately scoped to `crates/fr-store/src/lib.rs` and avoided the active peer-owned `fr-server` GET lane.

## Behavior Proof

- Byte-identical current/candidate TCP transcript: `f7679a51ed83f524e3597340ccf834e00cbe0cac09771608ecdba7bd97950a1d`
- Transcript covered SET overwrite, APPEND raw flag reset, COPY integer refcount, bit string raw flags, INCRBYFLOAT string flag, set/hash/zset sticky promotion flags, DUMP/RESTORE, and DEBUG DIGEST.
- Ordering, tie-breaking, floating-point behavior, RNG behavior, key hashing, command semantics, persistence payloads, and response bytes were unchanged.

## Gates

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p127_check cargo check -p fr-store --all-targets`: passed with pre-existing fr-store test warnings.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p127_test cargo test -p fr-store set_plain_borrowed -- --nocapture`: passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p127_clippy cargo clippy -p fr-store --lib -- -D warnings`: passed.
- `git diff --check -- crates/fr-store/src/lib.rs`: passed.
- `cargo fmt -p fr-store -- --check`: still blocked by pre-existing broad rustfmt drift outside this pass.

## Benchmark Result

Paired order, SET/P16/C50/1M:

- Current mean: `822.3 ms +/- 30.7 ms`, median `831.1 ms`
- Candidate mean: `860.4 ms +/- 124.9 ms`, median `826.1 ms`
- Hyperfine summary: current `1.05x +/- 0.16` faster.

Reversed order, SET/P16/C50/1M:

- Candidate mean: `898.4 ms +/- 83.9 ms`, median `887.7 ms`
- Current mean: `823.1 ms +/- 45.2 ms`, median `825.7 ms`
- Hyperfine summary: current `1.09x +/- 0.12` faster.

## Decision

Rejected. The behavior proof was clean, but the candidate failed the paired and reversed performance gate and scored below `2.0`. The source hunk was removed; evidence is retained here so this flag-layout family is not repeated.

Next route: avoid repeating per-entry flag packing and route deeper into the next live GET read-capsule or syscall/profile-backed child bead.
