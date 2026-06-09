# frankenredis-ohsk5.2 HSET Packed-Hash Single-Locate API Rejection

## Target

- Parent: `frankenredis-ohsk5`
- Pass: 90
- Baseline binary: `/tmp/codex-fr-ohsk5-2-baseline-target/release-perf/frankenredis`
  - sha256: `5098400bc15f71c26c54c5933963ac5ac32135d5c2ae8c3e9473b24410ee604c`
- Candidate binary: `/tmp/codex-fr-ohsk5-2-candidate-target/release-perf/frankenredis`
  - sha256: `aebf3784ec3d7d77c3084fecf63b134c25eca9761268594e0f9a9e14ead7954a`

Profile-backed target from pass 89:

- `Store::internal_entry`: 10.79% flat
- `PackedStrMap::insert`: 4.65% flat
- `foldhash::RandomState::hash_one::<&Vec<u8>>`: 4.22% flat
- `PackedStrMap::locate`: 2.26% flat
- Broad HSET P16 gap: FrankenRedis `511717.53 ops/sec` vs Redis `730918.21 ops/sec`

## Lever Tested

Add `HashFieldMap::insert_is_new` and `PackedStrMap::insert_is_new` so HSET can
derive the new-vs-overwrite integer reply from the same packed-record lookup
used for insertion. This is deeper than pass 89's call-site-only
`m.insert(...).is_none()` trial: it removes the packed-map promotion
`contains_key` scan for existing fields and avoids computing a discarded old
value for HSET.

The candidate preserved:

- HSET new-vs-overwrite integer replies
- HGETALL insertion order
- OBJECT ENCODING listpack/hashtable promotion behavior
- DEBUG DIGEST output
- wrongtype error ordering
- LFU/touch/digest side effects

## Behavior Proof

Raw RESP transcript comparison:

- JSON: `hset-golden-compare.json`
- SHA manifest: `hset-golden-compare.sha256`
- Baseline transcript sha256: `5bfc6446d8456bf1a2610a02eda03a88a3786b58bb11606bf9f98346931fa773`
- Candidate transcript sha256: `5bfc6446d8456bf1a2610a02eda03a88a3786b58bb11606bf9f98346931fa773`

Covered commands include HSET new/overwrite, HGET, HGETALL, HLEN, HEXISTS,
HSETNX, HDEL, OBJECT ENCODING, DEBUG DIGEST, wrongtype HSET, and missing-key
HGETALL. Ordering, tie-breaking, floating-point, and RNG are unchanged or not
applicable.

Candidate validation while applied:

- `rch exec -- cargo check -p fr-store --all-targets`: passed
- `rch exec -- cargo test -p fr-store insert_is_new -- --nocapture`: 2 passed
- `rch exec -- cargo test -p fr-store hset -- --nocapture`: HSET unit tests and
  metamorphic hash cases passed
- `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`: passed
- `rustfmt --edition 2024 --check crates/fr-store/src/packed_set.rs`: passed
- `cargo fmt -p fr-store -- --check`: still blocked by an unrelated committed
  formatting hunk near `crates/fr-store/src/lib.rs:22144`

## Benchmarks

Baseline before edit:

- HSET P16/300k baseline: `0.5981 s +/- 0.0238`

Paired HSET P16/300k:

- Baseline: `0.60085185096 s +/- 0.026465285495821635`
- Candidate: `0.5942649706600001 s +/- 0.01789456531147693`
- Summary: candidate `1.01x +/- 0.05`

Reversed HSET P16/1M:

- Candidate: `1.73510668626 s +/- 0.023866332216318432`
- Baseline: `1.8043301090100001 s +/- 0.03351858216091114`
- Summary: candidate `1.04x +/- 0.02`

Baseline-first HSET P16/1M:

- Baseline: `1.7809843621700001 s +/- 0.019655513654671348`
- Candidate: `1.7543096089199997 s +/- 0.042859135068909635`
- Summary: candidate `1.02x +/- 0.03`

## Decision

Rejected under the Score>=2.0 keep gate.

The larger 1M runs show a small directional win, but the 300k gate is only
`1.01x +/- 0.05` and the baseline-first 1M interval overlaps no-win territory.
Conservative score: impact `1.0`, confidence `2.5`, effort `2.0`, score `1.25`.

The source hunk and candidate-only tests were removed; no production source
change is retained.

## Next Primitive

Stop packed-HSET probe micro-levers. The next pass should use a different
primitive class from the no-gaps directive:

- zero-copy or arena-backed command packet execution that removes `Vec<Vec<u8>>`
  argv materialization across commands; or
- reply/output buffer batching if a fresh profile shifts write-side costs up.

Target ratio: `>=1.20x` on the selected P16 workload before any keep.
