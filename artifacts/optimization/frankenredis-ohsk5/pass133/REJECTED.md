# frankenredis-ohsk5.28 rejection record

## Target

- Bead: `frankenredis-ohsk5.28`
- Candidate: specialize borrowed bulk-string reply prefixes for one-digit payload lengths in `fr_protocol::encode_bulk_string_slice`.
- Profile basis: GET/P16/C50/3M server profile kept `fr_protocol::encode_bulk_string_slice` visible at 0.67% self, below `Store::get_string_bytes` at 2.60%, `process_buffered_frames` at 1.06%, `execute_plain_get_borrowed_into_with_default_read_gate` at 0.89%, and `refresh_client_memory_aggregates` at 0.76%.
- Alien mapping: deterministic zero-copy framing / TLV-style prefix specialization; no C dependencies, no unsafe.

## Behavior proof

- Focused candidate unit test passed:
  `rch exec -- cargo test -p fr-protocol borrowed_bulk_slice_encoder_matches_frame_encoder -- --nocapture`
- Candidate crate gates passed:
  `cargo check -p fr-protocol --all-targets`
  `cargo clippy -p fr-protocol --all-targets -- -D warnings`
  `cargo fmt -p fr-protocol -- --check`
- Golden TCP transcript covered SET/GET hits for lengths 0, 3, 9, 10, RESP2 nil, RESP3 nil, and QUIT.
- Request sha256: `a104fe95c906e257a14b926a5076fa662229e6a19cbafcc8aed1286d89825ad2`
- Current response sha256: `c2a907be49af2d8c1fa23ead45523bf30c3484adb29ced3030d9a6744856219f`
- Candidate response sha256: `c2a907be49af2d8c1fa23ead45523bf30c3484adb29ced3030d9a6744856219f`
- Ordering, tie-breaking, floating-point behavior, and RNG behavior were unchanged: the candidate only changed deterministic RESP bulk prefix bytes for one-digit lengths and emitted byte-identical output.

## Performance

- Baseline build: `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`; RCH fell back locally because no worker was admissible.
- Baseline GET/P16/C50/1M current-only hyperfine: `1.284 s +/- 0.651 s`, too noisy for a keep decision.
- Paired GET/P16/C50/1M:
  - Current: `840.7 ms +/- 59.4 ms`
  - Candidate: `826.3 ms +/- 38.4 ms`
  - Hyperfine summary: candidate `1.02x +/- 0.09` faster
- Reversed GET/P16/C50/1M:
  - Candidate: `793.2 ms +/- 53.3 ms`
  - Current: `747.0 ms +/- 57.6 ms`
  - Hyperfine summary: current `1.06x +/- 0.11` faster
- Confirm GET/P16/C50/3M:
  - Current hyperfine: `2.446 s +/- 0.261 s`
  - Candidate hyperfine: `2.181 s +/- 0.155 s`
  - Hyperfine summary: candidate `1.12x +/- 0.14` faster
  - Harness last-run current: `1,420,383 ops/sec`, p95 `827us`, p99 `1162us`
  - Harness last-run candidate: `1,385,288 ops/sec`, p95 `875us`, p99 `1478us`

## Decision

Rejected under the Score>=2.0 keep gate.

The benchmark direction is not stable: paired 1M weakly favored candidate, reversed 1M favored current, and the longer confirm split between hyperfine process time and the harness latency/ops report. Because the target row is only 0.67% self and the observed gain is not directionally reproducible, confidence is too low to keep source.

Score estimate: Impact `0.0` x Confidence `4.0` / Effort `1.0` = `0.0`.

The candidate source hunk was removed from `crates/fr-protocol/src/lib.rs`; only this evidence bundle remains.

## Next route

Do not repeat one-digit bulk-prefix specialization. Re-profile current main and route to a higher-mass primitive: store read/hash layout, borrowed GET read/accounting layout, or a structurally larger output/framing primitive that composes with `frankenredis-x5689` instead of racing it.
