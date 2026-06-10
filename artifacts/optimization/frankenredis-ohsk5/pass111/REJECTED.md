# Pass 111 Rejection: Static OK Reply for Borrowed Plain SET

## Target

- Bead: `frankenredis-ohsk5.9`
- Profile source: current main after pass110, SET/P16/C50/3M.
- Baseline throughput: `1485472.5714283248 ops/sec`, p50 `491us`, p95 `779us`, p99 `1004us`.
- Top self rows:
  - `fr_store::canonical_string_value_from_slice`: `6.33%`
  - `fr_protocol::parse_command_args_borrowed_into`: `1.74%`
  - `frankenredis::process_buffered_frames`: `1.29%`
  - `Store::set_plain_borrowed`: `1.17%`
  - writer/backtrace path: `1.06%`
  - `Runtime::plain_borrowed_default_key_write_allows`: `0.80%`

## Candidate

The trial changed only the borrowed plain SET success reply path:

- Runtime kept the existing `SET` side effects and reply-suppression bookkeeping.
- `fr-server` appended the static bytes `+OK\r\n` instead of constructing `RespFrame::SimpleString("OK".to_string())` and encoding it.

The source hunk was removed after the benchmark gate failed.

## Behavior Proof

- Baseline/candidate raw TCP RESP transcripts matched exactly.
- Golden sha256: `d467530ad9515c694924aaa721cf45fd1a8e90e700fe20944544abdac5d4f580`.
- Covered ordinary `SET`/`GET`, pipelined `SET` then `GET`, `CLIENT REPLY OFF`, suppressed borrowed plain `SET`, `CLIENT REPLY ON`, `PING`, and `GET` for the suppressed key.
- Ordering: command execution stayed serial on the same event-loop/runtime path.
- Tie-breaking: no sorted-set or ordering comparator touched.
- Floating-point: no float parsing or arithmetic touched.
- RNG: no random state, seeding, eviction sampling, or hash seed behavior touched.

## Validation

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p111_check1 cargo check -p fr-runtime -p fr-server --all-targets`: passed on `vmi1227854`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p111_test1 cargo test -p fr-runtime plain_set_borrowed_fast_path -- --nocapture`: passed on `vmi1227854`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p111_clippy1 cargo clippy -p fr-runtime -p fr-server --lib --bins --no-deps -- -D warnings`: passed on `vmi1227854`.
- `cargo fmt -p fr-runtime -p fr-server --check`: blocked by unrelated existing formatting drift in `fr-runtime`; no source hunk was retained.
- `ubs crates/fr-runtime/src/lib.rs crates/fr-server/src/main.rs`: nonzero from broad pre-existing inventory in the two large files; no reported finding landed on the candidate hunk.

## Benchmark Gate

Baseline-only SET/P16/C50/1M:

- `804.5 ms +/- 30.9 ms`

Paired order, baseline then candidate:

- Baseline: `810.6235664450001 ms +/- 36.403784335999334 ms`
- Candidate: `870.4407973200001 ms +/- 53.61090278073943 ms`
- Result: baseline `1.07x +/- 0.08` faster.

Reversed order, candidate then baseline:

- Candidate: `864.7929942150001 ms +/- 92.36958913478083 ms`
- Baseline: `831.125173465 ms +/- 79.18146370668805 ms`
- Result: baseline `1.04x +/- 0.15` faster.

## Score

- Impact: `0.0` because the candidate regressed the benchmark in both orders.
- Confidence: `3.0` because behavior proof passed and both orderings rejected the speed claim.
- Effort: `1.0`.
- Score: `0.0`, below the `>=2.0` keep gate.

## Route

Do not continue the static reply micro-family. The next pass should attack the measured value/key path with a structural primitive:

- small-value/key layout that avoids repeated canonicalization/probe work for borrowed plain SET;
- Swiss-table-style metadata/payload separation for hot key probes;
- or a request-scoped command packet/slab layout only if the next profile keeps argv/value materialization hot.
