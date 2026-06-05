# frankenredis-9mh3o Primitive Contract

## Target

Complete the packed small-collection encoding campaign by wiring zsets to a
small packed representation before promotion to the existing indexed skiplist
model.

## Alien Primitive

- Graveyard family: succinct data structures and cache-local contiguous layouts.
- Concrete artifact: `PackedZSet`, a single-buffer listpack-style sequence of
  `(member, score)` records sorted by the same `(canonical_score, member_bytes)`
  order as `SortedSet`.
- Expected win: small zsets avoid one hash-map entry, one BTreeMap node, rank
  treap state, and separately allocated member vectors per element.

## Baseline

- Remote baseline worker: `ts1`.
- Command: `cargo test -p fr-store zset_rank_treap_matches_oracle_and_reports_ab_ratio --profile release-perf -- --nocapture`.
- Baseline proof output: `old(rebuild)=460838332ns new(treap)=2030056ns ratio=227.01x`.
- Hyperfine artifact: `baseline-zrank-test-hyperfine.json`.
- Hyperfine mean: `794.9 ms +/- 47.6 ms` over 5 runs.
- Hyperfine sha256: `1cc67c21be2753ec583213a8fe9f944ea7be3c4b4f30136d794a24ad3788bfd6`.

## Final ZSET Wiring Harness

- Baseline command: `target-icywolf-9mh3o-baseline/release/fr-zset-pack-harness 8192 48`.
- Candidate command: `target-icywolf-9mh3o-candidate2/release/fr-zset-pack-harness 8192 48`.
- Paired hyperfine artifact: `comparison-hyperfine.json`.
- Baseline mean: `478.4 ms +/- 41.4 ms`.
- Candidate mean: `290.6 ms +/- 14.8 ms`.
- Ratio: `1.65x` faster.
- In-harness insert: `129912535ns` -> `79392893ns`.
- In-harness read: `137200388ns` -> `69667064ns`.
- In-harness pop: `6732322ns` -> `1781604ns`.
- Golden invariants: `state_digest=324091d9da416741` and
  `checksum=17527362544575379395` unchanged.
- Candidate zset proof output on `ts2`: `ZRANK ratio=222.07x`,
  `ZRANGE deep-index ratio=111.20x`.

## Isomorphism Obligations

- Ordering preserved: packed and indexed forms must both use canonical zero,
  `f64::total_cmp`, then member-byte ordering.
- Tie-breaking unchanged: equal scores must sort by member bytes ascending.
- Floating point: stored score bits must be unchanged except existing `-0.0` to
  `0.0` canonicalization.
- RNG: random-member and LFU sampling call counts must stay unchanged.
- Persistence: DUMP/RDB output type choice and member ordering must match the
  pre-change encoding rules.

## Validation

- `rch exec -- cargo test -p fr-store zset_promotes_when_listpack_limits_tighten_on_existing_update -- --nocapture`
- `rch exec -- cargo test -p fr-store zset --profile release-perf -- --nocapture`
- `rch exec -- cargo check -p fr-store --all-targets`
- `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`
- `cargo fmt -p fr-store -- --check`

## Fallback Trigger

Revert the zset wiring lever if focused zset tests, live differential checks, or
candidate hyperfine show a behavior change or Score < 2.0.
