# Pass 128 Rejected: GET Store Reply Capsule

- Bead: `frankenredis-ohsk5.26`
- Agent: `TealOtter`
- Closed: `2026-06-11T01:52:00Z`
- Target: GET/P16/C50 read path after the canonical GET packet keep.

## Baseline and Profile

- Current build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p128_current cargo build --profile release-perf -p fr-server -p fr-bench`
- Baseline GET/P16/C50/1M: `736.4 ms +/- 29.8 ms`
- Fresh profile workload: GET/P16/C50/3M
- Profile throughput: `1,756,354.830 ops/sec`
- Profile latency: p50 `418us`, p95 `641us`, p99 `869us`, p999 `1465us`, max `17935us`
- Profile rows supporting the trial: `Runtime::execute_plain_get_borrowed_into` `5.86%` children / `1.59%` self, `Store::drop_if_expired` `2.51%` children, `Store::get_string_bytes` `1.45%` children / `0.98%` self, `plain_borrowed_default_key_read_allows` `1.53%` children / `0.82%` self, `encode_bulk_string_slice` `0.98%` children / `0.62%` self.

## Candidate

Added a store-owned encoded GET reply capsule for the default non-LFU borrowed fast path. The candidate avoided the `record_keyspace_lookup` immutable probe followed by the `entries.get_mut` probe by doing one mutable lookup, encoding the GET reply while the value was live, and delegating LFU policies to the existing `get_string_bytes` path so RNG sampling stayed aligned.

The source hunk touched only `crates/fr-store/src/lib.rs` and `crates/fr-runtime/src/lib.rs`.

## Behavior Proof

- Byte-identical current/candidate TCP transcript: `0571c4fab0ef04fca624094ebdddbb7a1e0eff0384c1b857a01ea9fa1a6a2cff`
- Transcript covered string hit, integer hit, wrong-type error, expired miss with lazy delete, RESP2 nil, RESP3 nil via `HELLO 3`, and `DEBUG DIGEST`.
- Focused tests passed:
  - `cargo test -p fr-store write_get_string_bytes_reply_matches_get_stats_touch_and_expiry -- --nocapture`
  - `cargo test -p fr-runtime plain_get_borrowed_into_matches_frame_bytes_and_stats -- --nocapture`
- Isomorphism note: ordering, tie-breaking, floating-point behavior, RNG behavior, expiry propagation ordering, keyspace hit/miss stats, LRU/LFU semantics, commandstats/slowlog/latency/errorstats behavior, output suppression, and response bytes were preserved.

## Gates

- `cargo check -p fr-store -p fr-runtime --all-targets`: passed via rch remote, with known pre-existing test-helper warnings.
- `cargo clippy -p fr-store -p fr-runtime --lib -- -D warnings`: passed via rch remote.
- `git diff --check -- crates/fr-store/src/lib.rs crates/fr-runtime/src/lib.rs`: passed.
- `cargo fmt -p fr-store -p fr-runtime -- --check`: still blocked by pre-existing broad rustfmt drift in both files; the new test hunk was manually adjusted to match rustfmt.
- Candidate release build passed via rch remote.

## Benchmark Result

Paired GET/P16/C50/1M:

- Current mean: `748.3 ms +/- 51.3 ms`, median `747.6 ms`
- Candidate mean: `721.6 ms +/- 32.1 ms`, median `712.4 ms`
- Hyperfine summary: candidate `1.04x +/- 0.08` faster.

Reversed GET/P16/C50/1M:

- Candidate mean: `635.9 ms +/- 30.3 ms`, median `637.3 ms`
- Current mean: `711.6 ms +/- 37.9 ms`, median `713.1 ms`
- Hyperfine summary: candidate `1.12x +/- 0.08` faster.

Longer confirmation GET/P16/C50/3M:

- Current mean: `1.874 s +/- 0.155 s`, median `1.813 s`
- Candidate mean: `1.982 s +/- 0.082 s`, median `1.994 s`
- Hyperfine summary: current `1.06x +/- 0.10` faster.

## Decision

Rejected. The byte-level and unit-test behavior proof was clean, and the 1M paired/reversed benchmark was promising, but the longer 3M confirmation failed the performance gate and favored current. The source hunk was removed; evidence is retained here.

Next route: do not repeat store-owned GET reply capsules or single-probe GET read-capsule shapes. Attack a different profile-backed primitive, likely the default read-policy gate shape, command timing source strategy, or output/syscall path depending on the next fresh profile and live bead state.
