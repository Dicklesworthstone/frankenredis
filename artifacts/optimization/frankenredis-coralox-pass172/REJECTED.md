# frankenredis-ohsk5.42 rejected: packed HSET borrowed overwrite

Head: `1767753fd`
Bead: `frankenredis-ohsk5.42`

## Target

Current-main P16/C50 n300k command sweep selected HSET as the largest remaining
vs-upstream gap:

- FrankenRedis HSET: `599439.47 ops/sec`, p99 `5291us`
- Redis HSET: `885435.73 ops/sec`, p99 `1645us`
- Redis/fr ratio: `1.477x`
- LPUSH was tied, so the prior LPUSH route is no longer the active target.

Prior HSET micro-levers already rejected:

- borrowed mutation capsule
- packed-hash single-locate API
- borrowed parser/classifier reshaping

## Lever Tested

Candidate changed only `crates/fr-store/src/packed_set.rs`:

- `HashFieldMap::insert_borrowed` used a new `PackedStrMap::insert_borrowed`
  branch for packed hashes.
- Existing-field packed overwrite avoided allocating `field.to_vec()` and
  avoided copying out the discarded old value.
- New-field append and promotion conditions were preserved.

## Behavior Proof

Focused RCH test while the candidate was applied:

- `cargo test -p fr-store map_borrowed_insert_matches_owned_insert_observables -- --nocapture`
- Result: passed.

Golden TCP transcript:

- Input SHA256: `554c77e6b03bafecd550af42337a33c6e42793358a02d8c16d5adb17d4ec9bb4`
- Baseline output SHA256: `dedfbe2cd64737235a2fab54685917b37ee0147f81bc20588ce00590ebc56806`
- Candidate output SHA256: `dedfbe2cd64737235a2fab54685917b37ee0147f81bc20588ce00590ebc56806`
- Output length: `108` bytes for both.

Ordering/tie-breaking:

- Packed hash insertion order matched owned `insert` across new-field append and
  existing-field overwrite.
- HGETALL field order in the transcript was byte-identical.

Floating-point/RNG:

- No floating-point path touched.
- No RNG path touched; LFU gating and random sampling remained outside the
  packed-map primitive and unchanged.

## Benchmarks

Current baseline HSET P16/C50 n1M:

- `1.332798501s +/- 0.029662793s`

Paired HSET P16/C50 n1M:

- Baseline: `1.369392387s +/- 0.054223621s`
- Candidate: `1.374551518s +/- 0.064135553s`
- Hyperfine summary: baseline `1.00 +/- 0.06x` faster than candidate.

Score:

- Impact `0.0`
- Confidence `0.8`
- Effort `1.0`
- Score `0.0`, below the required `2.0`.

## Decision

Rejected. The source hunk and candidate-only test were removed before commit.

Next route:

- Stop packed HSET overwrite micro-levers.
- Attack a larger HSET batch-shape primitive: parse/execute repeated
  `HSET key field value` pipeline groups with batch-level command metadata and
  reply construction while preserving serial side effects, output ordering,
  active-expire/dirty/stat/slowlog semantics, and golden SHA256.
