# Pass 112 Keep: Direct Borrowed Plain SET Packet Parser

## Target

- Bead: `frankenredis-ohsk5.10`
- Baseline: current main `451a53d6e`, release-perf `fr-server` + `fr-bench`.
- Baseline SET/P16/C50/1M hyperfine: `795.4 ms +/- 39.4 ms`.
- Baseline SET/P16/C50/3M profile: `1546028.9091554063 ops/sec`, p50 `473us`, p95 `735us`, p99 `948us`, 2620 samples, 0 lost.
- Top baseline self rows:
  - `fr_store::canonical_string_value_from_slice`: `4.78%`
  - `fr_protocol::parse_command_args_borrowed_into`: `1.85%`
  - `Runtime::plain_borrowed_default_key_write_allows`: `1.42%`
  - `Store::set_plain_borrowed`: `1.37%`
  - `Runtime::execute_plain_set_borrowed`: `1.32%`
  - `frankenredis::process_buffered_frames`: `0.86%`

## Lever

Added a conservative direct parser for exact borrowed plain SET packets in
`fr-server`:

- accepts only canonical strict RESP `*3\r\n$3\r\nSET\r\n$key\r\n$value\r\n`
  shape, case-insensitive for `SET`;
- checks the same `ParserConfig` array and bulk limits before firing;
- falls back to the existing borrowed parser for all non-canonical, malformed,
  option-bearing, disabled, or unsupported cases;
- keeps `Runtime::execute_plain_set_borrowed` and `Store::set_plain_borrowed`
  unchanged.

This is the region/command-packet lever harvested from the graveyard direction:
avoid per-frame borrowed argv packet allocation and helper-chain dispatch for
the hottest exact SET packet while keeping the generic parser as the semantic
owner for every other packet.

## Behavior Proof

- Baseline/candidate raw TCP RESP transcripts matched exactly.
- Golden sha256: `27fde3960948e19fe73956e617c34b409981279579471ef94feb6af1ebe6e30e`.
- Covered mixed-case direct SET, GET, option-bearing SET NX fallback, pipelined
  direct SET ordering, `CLIENT REPLY OFF`, suppressed direct SET, `CLIENT REPLY
  ON`, PING, and GET for the suppressed key.
- Ordering: command execution remains serial in the same `process_buffered_frames`
  loop and calls the same runtime function.
- Tie-breaking: no ordered data structure comparator touched.
- Floating-point: no float parsing/arithmetic touched.
- RNG: no random state, hash seed, sampling, or eviction RNG behavior touched.

## Validation

- `cargo fmt -p fr-server -- --check`: passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p112_check2 cargo check -p fr-server --all-targets`: passed through `rch` local fallback.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p112_test1 cargo test -p fr-server borrowed_plain_set_packet_parser -- --nocapture`: passed, 2 focused tests.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p112_clippy1 cargo clippy -p fr-server --lib --bins --no-deps -- -D warnings`: passed through `rch` local fallback.
- `ubs crates/fr-server/src/main.rs`: nonzero from broad pre-existing full-file inventory; no new hunk-specific finding. UBS embedded fmt/clippy/check/test-build gates were clean.

## Benchmark Gate

SET/P16/C50/1M paired order:

- Baseline: `735.750824265 ms +/- 29.131726867880816 ms`
- Candidate: `704.5304948900001 ms +/- 15.204021218523428 ms`
- Candidate: `1.04x +/- 0.05`

SET/P16/C50/1M reversed order:

- Candidate: `704.7920910600001 ms +/- 29.056859734861662 ms`
- Baseline: `746.70615706 ms +/- 37.13168967570122 ms`
- Candidate: `1.06x +/- 0.07`

SET/P16/C50/3M paired confirmation:

- Baseline: `2.22207236446 s +/- 0.20867533409437028 s`
- Candidate: `2.1527709264599997 s +/- 0.31727413768726404 s`
- Candidate: `1.03x +/- 0.18`

SET/P16/C50/3M reversed confirmation:

- Candidate: `2.10613910318 s +/- 0.07102187450059651 s`
- Baseline: `2.21237472518 s +/- 0.025159945948830024 s`
- Candidate: `1.05x +/- 0.04`

## Score

- Impact: `2.0` for a repeated 3-6% SET/P16 hot-path win.
- Confidence: `3.0` because all four mean comparisons favor candidate, golden
  proof is byte-identical, and the longer reversed confirmation is clean.
- Effort: `1.0`.
- Score: `6.0`, above the `>=2.0` keep gate.

## Post-Keep Profile

Candidate SET/P16/C50/3M post-profile:

- `1546101.0393582808 ops/sec`, p50 `458us`, p95 `767us`, p99 `1134us`, 2611 samples, 0 lost.
- Top self rows:
  - `fr_store::canonical_string_value_from_slice`: `5.99%`
  - `Store::set_plain_borrowed`: `1.34%`
  - `frankenredis::process_buffered_frames`: `1.17%`
  - `Runtime::execute_plain_set_borrowed`: `0.91%`
  - `Runtime::plain_borrowed_default_key_write_allows`: `0.91%`
  - `foldhash::quality::RandomState::hash_one::<&[u8]>`: `0.78%`

Next route: the parser row is reduced below the dominant value/key work. The
next pass should attack value canonicalization or key/hash/probe layout as a
different structural primitive, not another direct command parser for SET.
