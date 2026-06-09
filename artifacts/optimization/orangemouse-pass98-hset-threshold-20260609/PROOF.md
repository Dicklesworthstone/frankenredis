# frankenredis-ohsk5 HSET Singleton HashFieldMap Rejection

## Target

- Parent bead: `frankenredis-ohsk5`
- Commit under test: `b59399522`
- Profile-backed hotspot:
  - pass93 HSET P16/1M: `PackedStrMap::locate` 3.92%, `HashFieldMap::insert` 2.77%
  - pass94 HSET P16/1M: `PackedStrMap::locate` 3.13%, `PackedStrMap::insert` 2.80%
- Baseline server SHA256:
  - `83d79c3f1cff0599027b96262dab48682721100bdc8957b96e6e93583e80ab40`
- Candidate server SHA256:
  - `7517b8a9390094a1e411de8bdef7970acca12197446bfc424edae85a5a9dd6b1`

## Lever Tested

Add a `HashFieldMap::Single { field, value }` storage state for the common
single-field hash shape used by the HSET benchmark. The candidate bypassed the
packed listpack scan/varint/splice path for repeated `HSET key field value`
updates until a second field or an oversized first field required the existing
packed/hash storage states.

This was intentionally deeper than a predicate reorder or single extra lookup
avoidance: it changed the small-hash storage state machine while preserving the
existing `HashFieldMap` API.

## Behavior Proof

Focused candidate gates while the hunk was applied:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass98-candidate-target cargo check -p fr-store --all-targets`: passed on `ovh-b`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass98-candidate-target cargo test -p fr-store packed_set -- --nocapture`: passed on `vmi1227854`
  - 12 packed-set tests passed, including `map_equivalent_to_indexmap`
  - new singleton test covered same-field update, second-field promotion to packed storage, removal, and oversized first-field promotion to hash storage
- `rustfmt --edition 2024 --check crates/fr-store/src/packed_set.rs`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass98-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`: passed on `ovh-b`

Golden HSET transcript:

- Raw RESP baseline SHA256: `b8fe5869f3dd56c6e630ea6cc546cbf151c6ed71c40d970a2f6bf752bace41b7`
- Raw RESP candidate SHA256: `b8fe5869f3dd56c6e630ea6cc546cbf151c6ed71c40d970a2f6bf752bace41b7`
- Manifest: `hset-golden-compare.json`

The golden battery byte-compared HSET new-vs-overwrite replies, HGETALL field
ordering, HSETNX behavior, HDEL survivor ordering, OBJECT ENCODING, DEBUG
DIGEST, wrongtype ordering, and missing-hash HGETALL. Tie-breaking,
floating-point behavior, and RNG are not involved in this storage path.

## Benchmarks

Baseline before edit:

- HSET P16/300k: `0.55939060014 s +/- 0.02095918550`

Paired HSET P16/300k:

- Baseline: `0.63995407692 s +/- 0.03568923651`
- Candidate: `0.60878002759 s +/- 0.03155590116`
- Summary: candidate `1.05x +/- 0.08`

Reversed HSET P16/1M:

- Candidate: `1.77630363762 s +/- 0.08945802809`
- Baseline: `1.80358581232 s +/- 0.06406059624`
- Summary: candidate `1.02x +/- 0.06`

## Decision

Rejected under the Score>=2.0 keep gate.

The candidate is behavior-clean, but the measured effect is small and noisy:
`1.05x +/- 0.08` on the paired 300k gate and `1.02x +/- 0.06` on the longer
reversed 1M confirmation. Conservative score: impact `1.0`, confidence `2.0`,
effort `2.0`, score `1.0`.

No production source hunk is retained.

## Next Primitive

Stop packed-HSET storage micro-family work. The next attack should be a
fundamentally different primitive from the no-gaps directive:

- zero-copy/reused argv packet execution that removes hot-path `Vec<Vec<u8>>`
  materialization across HSET/SET-family dispatch; or
- reply/output buffer batching that writes integer/simple replies directly from
  a reusable per-client packet.

Target ratio before keep: `>=1.20x` on the selected P16 workload.
