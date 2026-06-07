# Pass 64 Inline Bulk Rejection

## Target

- Bead: `frankenredis-5hcrx` follow-up under the pass 64 GET profile.
- Baseline commit: `b5ab1a76c`.
- Profile-backed hotspot: GET P16/1M server profile showed `Value::string_owned`
  and allocation under 3-byte GET replies after direct encoding and output-cursor
  variants failed the keep gate.

## Lever

Added an internal `RespFrame::BulkStringInline` representation and
`Store::get_read_value` path so short stored string values could be carried to
the normal encoder without allocating a `Vec<u8>`.

The patch is preserved in `candidate-inline-bulk.patch`; it was not kept in
production source.

## Behavior Proof

- Raw TCP RESP transcript covered `FLUSHALL`, small string GET, integer-looking
  string GET, 15-byte inline-cap boundary, 16-byte owned boundary, missing GET,
  wrong-type GET, and `QUIT`.
- Baseline, candidate, and vendored Redis oracle emitted byte-identical
  transcripts.
- Golden SHA-256:
  `77b1fb0a092c82d445d128c3571e82e83717ce2b6e5f152b5944594179160c56`.
- Ordering, tie-breaking, floating-point, and RNG behavior are not touched by
  this representation-only GET reply change. Store expiry/touch and command
  stats stayed on the existing GET fast-path flow.

## Validation

- `rch exec -- env CARGO_TARGET_DIR=/tmp/fr-pass64-candidate-check-target cargo check -p fr-protocol -p fr-store -p fr-command -p fr-runtime -p fr-server --all-targets`
  passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/fr-pass64-candidate-test-target cargo test -p fr-protocol golden_inline_bulk_matches_owned_bulk -- --nocapture`
  passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/fr-pass64-candidate-test-target cargo test -p fr-runtime plain_get_borrowed_fast_path_matches_generic_hit_miss_stats -- --nocapture`
  passed.

## Benchmarks

- GET P16/300k paired, 7 runs:
  - baseline `0.45031837865714286 s +/- 0.005887547734974754`
  - candidate `0.4446801703714286 s +/- 0.008216048113770039`
  - candidate `1.01x +/- 0.02`, inside noise.
- GET P16/1M reversed, 5 runs:
  - candidate `1.4176788166399998 s +/- 0.013851965563839982`
  - baseline `1.35967711624 s +/- 0.04184465945508323`
  - baseline `1.04x +/- 0.03` faster.

## Decision

Reject under Score>=2.0. Impact is not credible across the larger reversed run,
so the source patch was removed from the shared tree.

Next route is pass65: attack a deeper store/key-layout primitive, specifically
an epoch-validated hot-key GET read certificate only after fresh key-locality and
profile dominance evidence.
