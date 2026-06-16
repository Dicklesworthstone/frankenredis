# pass219: frankenredis-uybhq compact full-zset order

Bead: `frankenredis-uybhq`

## Target

Profile-backed residual after `c4417d55e` shared full-zset member bytes with
`Arc<[u8]>`: fresh-process `scripts/zset_memory_profile.py` still measured
FrankenRedis at `58.23 MB` / `153 B-member` versus Redis `37.78 MB` /
`99 B-member` on `500 keys x 800 small members`.

The remaining target was structural overhead in the full-zset ordered index.

## Lever

One source lever in `crates/fr-store/src/lib.rs`: replace the unconditional
`BTreeMap<ScoreMember, ()>` full-zset order index with `FullZSetOrder`.
Medium full zsets (`<= 2048` entries) use a sorted `Vec<ScoreMember>`; larger
sets promote to the existing tree representation.

The lookup/random-selection `IndexMap` remains unchanged, so `ZRANDMEMBER`
distribution is unchanged.

## Before / After

Release-perf binaries:

- Baseline FrankenRedis: `14344058517e0b30cc09463b67eb1810a5f76ddf7409eb209193b2141d5ab469`
- Candidate FrankenRedis: `738f7241f9052ba522463d8c7b850d625cec1f090199230c23838f7ed5431c03`
- Redis oracle: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`

Fresh-process zset RSS profile, `500 keys x 800 small members = 400000`:

- Baseline: Redis `37.78 MB`, FR `58.23 MB`, ratio `1.54x`
- Candidate: Redis `37.83 MB`, FR `47.27 MB`, ratio `1.25x`
- FR data-RSS improvement: `58.23 / 47.27 = 1.23x`
- FR bytes/member: `153 -> 124`

ZADD guardrail, `redis-benchmark -t zadd -n 300000 -r 1000000 -c 50 --seed 424242`:

- Baseline: `66563.12 req/s`, p50 `0.351 ms`, p95 `0.543 ms`, p99 `1.215 ms`
- Candidate: `66122.98 req/s`, p50 `0.359 ms`, p95 `0.583 ms`, p99 `0.951 ms`

## Isomorphism Proof

- Ordering/tie-breaking: `ScoreMember::Ord` is unchanged: canonicalized score
  total order, then `MemberPart` lex order. The compact order stores exactly
  that sorted key sequence.
- Floating point: score canonicalization, `total_cmp`, infinities, and `-0`
  behavior are unchanged.
- RNG: `ZRANDMEMBER` still resolves random indices through the unchanged
  `IndexMap`; the order index is not part of random selection.
- Raw golden transcript: Redis 7.2.4, baseline FR, and candidate FR matched
  byte-for-byte for ZADD/range/lex/count/rank/DUMP/DEBUG DIGEST/pop/remove
  commands.
- Golden SHA256:
  `236d95a41c7c140172773e3d382dbaf63aab44e3de1e759c3ceb9632dfec02c5`.
- Differential fuzz: `scripts/zset_differ.py --iters 8000 --seed 150219`
  passed with no Redis divergence.

## Gates

- `rch exec -- env CARGO_TARGET_DIR=... cargo test -p fr-store zset -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=... cargo clippy -p fr-store --all-targets -- -D warnings`
- `rch exec -- env CARGO_TARGET_DIR=... cargo build --profile release-perf -p fr-server`
- `cargo fmt --check -p fr-store`
- `ubs crates/fr-store/src/lib.rs` completed and remains nonzero on existing
  monolithic-file inventory; the new direct-slice finding from the first scan
  was fixed, and the final targeted grep found no `FullZSetOrderRange` /
  `keys[start..end]` finding.

## Score

Impact `2.0` (large RSS reduction on the profile target) x Confidence `4.0`
/ Effort `1.5` = `5.33`; kept.
