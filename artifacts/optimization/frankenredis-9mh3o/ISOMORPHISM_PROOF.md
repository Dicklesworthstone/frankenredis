# frankenredis-9mh3o Isomorphism Proof

## Lever

Wire `PackedZSet` into `SortedSet` for small zsets and add a missing-key ZADD
bulk construction path that directly builds the packed sorted buffer.

## Ordering And Tie-Breaking

- Packed and full zsets both sort by canonicalized score, then member bytes.
- Equal scores keep the existing Redis-observable member-byte ascending order.
- `PackedZSet::from_unique_pairs` sorts with the same `zset_cmp` helper used by
  insertion and by the full `ScoreMember` ordering contract.

## Floating Point

- Score storage still canonicalizes `-0.0` to `0.0`.
- Non-zero score bits are copied unchanged into the packed buffer.
- Score comparisons still use `f64::total_cmp` through the existing canonical
  score path.

## RNG And Side Effects

- No random API or LFU sampling path was changed.
- Existing-key `ZADD` still walks input pairs sequentially, so duplicate member
  processing, `CH`, `NX`, `XX`, `GT`, and `LT` side effects remain in command
  order.
- Missing-key `ZADD` deduplicates only for final storage construction; it still
  counts duplicate member score changes for the dirty counter before insertion.

## Golden State

Baseline:

```text
state_digest=324091d9da416741
checksum=17527362544575379395
memory_before_pop=8449664
memory_after_pop=8317440
```

Candidate:

```text
state_digest=324091d9da416741
checksum=17527362544575379395
memory_before_pop=8449664
memory_after_pop=8317440
```

## Benchmark Delta

- Paired hyperfine baseline: 463.7 ms +/- 16.0 ms.
- Paired hyperfine candidate: 291.1 ms +/- 9.5 ms.
- Ratio: 1.59x faster.
- Direct phase deltas:
  - Insert: 129,912,535 ns -> 73,739,467 ns.
  - Read: 137,200,388 ns -> 74,012,895 ns.
  - Pop: 6,732,322 ns -> 2,087,022 ns.

Score gate: 2.52 = 1.59 impact x 0.95 confidence / 0.60 effort.

## Validation

- `rch exec -- env CARGO_TARGET_DIR=target-icywolf-9mh3o-check3 cargo check -p fr-store`
- `rch exec -- env CARGO_TARGET_DIR=target-icywolf-9mh3o-test-zadd cargo test -p fr-store zadd_repeated_member_processes_pairs_sequentially -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-icywolf-9mh3o-test-zrange2 cargo test -p fr-store zrange -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-icywolf-9mh3o-test-packed cargo test -p fr-store packed_set::tests::zset -- --nocapture`
- `cargo fmt -p fr-store -- --check`
- `rch exec -- env CARGO_TARGET_DIR=target-icywolf-9mh3o-clippy2 cargo clippy -p fr-store --all-targets -- -D warnings`
- `rch exec -- env CARGO_TARGET_DIR=target-icywolf-9mh3o-test-packed-after-ubs cargo test -p fr-store packed_set::tests::zset -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-icywolf-9mh3o-check-after-ubs cargo check -p fr-store --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=target-icywolf-9mh3o-clippy-after-ubs cargo clippy -p fr-store --all-targets -- -D warnings`
- `ubs crates/fr-store/src/packed_set.rs`
