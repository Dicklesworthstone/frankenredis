# frankenredis-ohsk5.1 HSET Packed-Hash Single-Probe Rejection

## Target

- Parent: `frankenredis-ohsk5`
- Pass: 89
- Profile-backed hotspot: HSET P16 standard workload.
- Baseline binary: `/tmp/codex-fr-pass89-current-target/release-perf/frankenredis`
  - sha256: `aacdb33dec1c69d267c9b984584e93ba59a3e74dd5153e933ed1c8940d3a24e8`
- Candidate binary: `/tmp/codex-fr-ohsk5-1-candidate-target-scratch/release-perf/frankenredis`
  - sha256: `dc51c70c83389b4d2783fc9201c7d1a44969387bae2ea39e3cb176d4958da3ab`

Current-main HSET profile showed:

- `Store::internal_entry`: 10.79% flat
- `Runtime::refresh_store_runtime_info_context`: 6.13% flat
- `PackedStrMap::insert`: 4.65% flat
- `foldhash::RandomState::hash_one::<&Vec<u8>>`: 4.22% flat
- `PackedStrMap::locate`: 2.26% flat

The broad P16 sweep showed HSET as the largest current standard gap:
FrankenRedis `511717.53 ops/sec` vs Redis `730918.21 ops/sec`, ratio `0.7001`.

## Lever Tested

Collapse HSET's packed-hash field check from:

```rust
let is_new = !m.contains_key(&field);
m.insert(field, value);
```

to:

```rust
let is_new = m.insert(field, value).is_none();
```

This was a one-line candidate intended to avoid a redundant packed-map field
scan while preserving HSET integer replies, overwrite behavior, insertion order,
one-way encoding promotion, digest invalidation, LFU/touch side effects, and
wrongtype/error ordering.

## Behavior Proof

Raw RESP transcript comparison:

- JSON: `hset-golden-compare.json`
- SHA manifest: `hset-golden-compare.sha256`
- Baseline transcript sha256: `5bfc6446d8456bf1a2610a02eda03a88a3786b58bb11606bf9f98346931fa773`
- Candidate transcript sha256: `5bfc6446d8456bf1a2610a02eda03a88a3786b58bb11606bf9f98346931fa773`

Covered commands include HSET new/overwrite integer replies, HGET, HGETALL
ordering, HLEN, HEXISTS, HSETNX, HDEL, OBJECT ENCODING, DEBUG DIGEST,
wrongtype HSET, and missing-key HGETALL.

Ordering, tie-breaking, floating-point, and RNG are unchanged or not applicable.

Candidate validation while the hunk was applied:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-ohsk5-1-candidate-check cargo check -p fr-store --all-targets`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-ohsk5-1-candidate-test cargo test -p fr-store hset -- --nocapture`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-ohsk5-1-candidate-clippy cargo clippy -p fr-store --all-targets -- -D warnings`: passed

`cargo fmt -p fr-store -- --check` is already blocked on an unrelated committed
formatting hunk near `crates/fr-store/src/lib.rs:22144`; the candidate hunk itself
does not change formatting shape.

## Benchmarks

Baseline before edit:

- `hset-p16-300k-baseline-hyperfine.json`
- Mean: `0.5945 s +/- 0.0262 s`

Paired HSET P16 300k:

- Baseline: `0.7639737373600001 s +/- 0.14092962362941677`
- Candidate: `0.6345946802600001 s +/- 0.03276802245951639`
- Summary: candidate `1.20x +/- 0.23` faster

Reversed HSET P16 1M:

- Candidate: `1.9591959746800003 s +/- 0.47642311845593666`
- Baseline: `2.0125851848050003 s +/- 0.1788034443779968`
- Summary: candidate `1.03x +/- 0.27` faster

## Decision

Rejected under the Score>=2.0 gate.

The first paired run was directionally positive but noisy; the reversed 1M
confirmation collapsed to `1.03x +/- 0.27`, so confidence is too low to keep a
micro-lever. Conservative score: impact `1.03`, confidence `0.4`, effort `1.0`,
score `0.41`.

No production source change is retained from this lever.

## Next Primitive

Stop repeating HSET contains-key micro-tweaks. Re-profile the standard P16 suite
first, then attack a structurally different primitive such as zero-copy RESP /
command-packet routing, per-readable-batch arena/slab reuse, or the next largest
fresh write-path gap. Only return to hash storage if the fresh profile supports a
non-micro `HashFieldMap`/`PackedStrMap` entry API with a `>=1.20x` target and low
variance before any keep.
