# pass225 proof: pack Entry LFU minutes and sticky flags

Bead: `frankenredis-uhthd`

Lever: keep the hot `Entry` tail compact after the pass224 expiry-side-dict
change by storing `lfu_last_touch_min` as a saturating `u32` minute clock and
packing seven sticky object-encoding / COPY-refcount booleans into one `u8`.

## Baseline

Baseline binary: `target-coralox-pass224-final/release-perf/frankenredis`

Fresh-process persistent keyspace RSS, 1,000,000 keys, pipeline 256:

- RSS delta: `235880 KiB`
- Bytes per key: `241.541120`
- Load seconds: `18.073121598`

## Candidate

Candidate binary: `target-coralox-pass225/release-perf/frankenredis`

Fresh-process persistent keyspace RSS, 1,000,000 keys, pipeline 256:

- RSS delta: `219424 KiB`
- Bytes per key: `224.690176`
- Load seconds: `18.586804894`

Delta:

- RSS: `-16456 KiB`
- Bytes per key: `-16.850944`
- Percent RSS reduction: `-6.98%`
- `Entry` layout proof: `56B -> 48B`

## Hyperfine

Paired 300,000-key load using the same local ts1-offline harness:

- Baseline mean: `6.0193641738s +/- 0.8099537264`
- Candidate mean: `5.1035673604s +/- 0.0601020653`
- Hyperfine ratio: candidate `1.18x +/- 0.16` faster

## Isomorphism proof

- Object encoding flags are represented by the same seven logical bits as the
  previous seven booleans. Every set/clear/copy/reset site is preserved through
  `Entry::has_flag`, `Entry::set_flag`, and `Entry::clear_entry_flags`.
- `COPY` still preserves source encoding metadata and still marks copied
  int-encoded strings as private with `ENTRY_INT_COPY_NOT_SHARED`.
- Integer whole-entry writes still clear all sticky encoding/refcount metadata.
- LFU elapsed-minute arithmetic remains monotone and saturating. `u32::MAX`
  minutes is roughly 8171 years, beyond any Redis-observable horizon in this
  server. Decay still computes elapsed minutes with saturating subtraction.
- SCAN ordering, sorted-set tie-breaking, floating-point handling, and RNG paths
  are not touched by this lever.

Golden baseline-vs-candidate transcript:

- SHA-256: `3d6351d7fd9c69ca31a41b045096adc93ce8918b84324f8cff73867a0d6e6287`
- Covers: object encodings/refcount, COPY, integer rewrite, INCRBYFLOAT,
  APPEND, SETRANGE, SETBIT, BITFIELD, set/hash/zset sticky promotion, PFADD,
  PFMERGE, SCAN, and DUMP/RESTORE encoding preservation.

Redis-oracle gates:

- `python3 scripts/object_encoding_boundary_gate.py --bin target-coralox-pass225/release-perf/frankenredis --redis-bin legacy_redis_code/redis/src/redis-server`
  - PASS: OBJECT ENCODING byte-exact across 23 boundary shapes.
- `python3 scripts/restore_encoding_differ.py --oracle <redis_port> --fr <candidate_port>`
  - PASS: original and post-RESTORE OBJECT ENCODING byte-exact.
- `python3 scripts/object_policy_differ.py <redis_port> <candidate_port>`
  - PASS: OBJECT policy-gating byte-exact across 6 policies x 7 subcommands.

## Local gates

- `CARGO_TARGET_DIR=target-coralox-pass225 cargo check -j1 -p fr-store --all-targets`
- `CARGO_TARGET_DIR=target-coralox-pass225 cargo clippy -j1 -p fr-store --all-targets -- -D warnings`
- `cargo fmt -p fr-store -- --check`
- `CARGO_TARGET_DIR=target-coralox-pass225 cargo build -j1 -p fr-server --profile release-perf`
- `CARGO_TARGET_DIR=target-coralox-pass225 cargo test -j1 -p fr-store value_size_is_capped_by_boxing_sortedset -- --nocapture`
- `CARGO_TARGET_DIR=target-coralox-pass225 cargo test -j1 -p fr-store object_encoding -- --nocapture`
- `CARGO_TARGET_DIR=target-coralox-pass225 cargo test -j1 -p fr-store object_refcount -- --nocapture`
- `CARGO_TARGET_DIR=target-coralox-pass225 cargo test -j1 -p fr-store object_freq -- --nocapture`
- `CARGO_TARGET_DIR=target-coralox-pass225 cargo test -j1 -p fr-store incrby_existing_key_matches_whole_entry_replacement_side_effects -- --nocapture`
- `CARGO_TARGET_DIR=target-coralox-pass225 cargo test -j1 -p fr-store incr_invalid_integer_leaves_entry_and_side_effects_unchanged -- --nocapture`
- `CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass225-test-target cargo test -j1 -p fr-store --lib -- --nocapture`
  - PASS: `634 passed; 2 ignored`
- `git diff --check` on the changed paths

`ubs crates/fr-store/src/lib.rs` remains nonzero on pre-existing broad
whole-file findings, while the embedded fmt, clippy, cargo check, and test-build
sections are clean. No pass225-specific scanner finding was identified.

Score: `Impact 3 * Confidence 4 / Effort 2 = 6.0`.
