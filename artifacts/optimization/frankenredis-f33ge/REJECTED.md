# frankenredis-f33ge rejected: inline small hashtable set members

## Target

- Bead: `frankenredis-f33ge`
- Profile-backed hotspot: pass180 current-main P16/C50 sweep selected SADD as
  the top remaining residual:
  - Redis: `943396 req/s`
  - FrankenRedis: `751879 req/s`
  - Redis/fr ratio: `1.25x`
- Alien-graveyard primitive: inline small objects. The tested lever replaced
  `IndexSet<Vec<u8>>` hash-encoded generic set members with an enum that stores
  members up to 23 bytes inline and falls back to `Vec<u8>` for larger members.

## Baseline

- Existing RCH-built baseline binaries:
  - `target-coralox-pass180-baseline/release-perf/frankenredis`
  - `target-coralox-pass180-baseline/release-perf/fr-bench`
- Baseline artifacts retained in `artifacts/optimization/frankenredis-coralox-pass180/`.
- Baseline SADD command-level row from the sweep: FrankenRedis `751879 req/s`
  vs Redis `943396 req/s`.

## Behavior Proof

- Candidate RCH gates passed:
  - `cargo test -p fr-store generic_hash_set_inline_members_preserve_indexset_semantics -- --nocapture`
  - `cargo check -p fr-store --all-targets`
  - `cargo build --profile release-perf -p fr-server -p fr-bench`
- Local hygiene passed:
  - `cargo fmt --package fr-store -- --check`
  - `git diff --check -- crates/fr-store/src/packed_set.rs`
- Golden raw TCP transcript exercised hash-encoded generic set behavior with
  more than 128 string members, duplicate SADD, SCARD, SISMEMBER,
  deterministic `SRANDMEMBER s 0`, deterministic `SPOP s 0`, SMEMBERS, DUMP,
  and QUIT.
- Golden SHA256:
  - input: `96e2d3b02a940e96b18ff52e9802e1d4ee95b88e8bb9529d301f5625aadcf66e`
  - baseline output: `cf30bcec868aed0590b3a98a66c7256bff1b6e74426a1e22b9fb19dfea5670eb`
  - candidate output: `cf30bcec868aed0590b3a98a66c7256bff1b6e74426a1e22b9fb19dfea5670eb`
  - output size: `2267` bytes for both baseline and candidate
- Isomorphism:
  - Ordering: `IndexSet` insertion order was preserved; SMEMBERS bytes matched.
  - Tie-breaking: no sorted-score or equal-score paths touched.
  - Floating point: no FP paths touched.
  - RNG: the candidate changed member storage only. `get_index` and
    `pop_index` kept the same chosen-index contract; random index generation was
    untouched. The golden used zero-count random commands to keep transcript
    bytes deterministic.

## Re-benchmark

- Paired SADD P16/C50/n1M with vendored `redis-benchmark`, fixed seed `123`,
  fresh server per run:
  - baseline: `1.426s +/- 0.032`
  - candidate: `1.417s +/- 0.081`
  - hyperfine summary: candidate `1.01x +/- 0.06` faster than baseline
- Score:
  - `0.3 impact * 1.0 confidence / 1.0 effort = 0.3`
  - Fails the required `Score>=2.0` keep gate.

## Decision

- Rejected.
- Production source hunk and candidate-only test were removed before commit.
- Evidence retained in this directory.

## Next Route

Do not repeat inline-small `IndexSet` member storage for SADD. Re-profile
current main and choose a deeper SADD primitive only if SADD remains top:
batch-level set insertion, command packet/arena metadata, or a different
hashtable/probe layout with a stronger profile row.
