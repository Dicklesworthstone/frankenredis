# frankenredis-mn0qm pass145 proof

## Target

Profile-backed bead: `frankenredis-mn0qm`

Hot path: cold repeated deep-index sorted-set reads:

```text
ZRANGE key 90000 90000
```

on a 100k-member zset, before any explicit rank query warms the existing lazy
order-statistic treap.

## Lever

One lever only: adaptive lazy order-statistic materialization for repeated deep
by-index `ZRANGE`/`ZREVRANGE` reads, backed by an O(n) Cartesian treap builder
from the authoritative `BTreeMap<ScoreMember, ()>` ordering.

Write-path resets were removed after the first candidate showed noisy ZADD
cost. The final candidate leaves cold one-shot behavior effectively unchanged
and warms the treap only after repeated deep, narrow by-index reads.

## Baseline

Built through RCH on worker `vmi1152480` using:

```text
CARGO_TARGET_DIR=target-mn0qm-baseline-harness cargo build --release --manifest-path artifacts/optimization/frankenredis-mn0qm/pass145/harness/Cargo.toml
```

Direct baseline:

```text
cold-range elapsed_ns=1857055898 checksum=245608431828199426492213436369610368752
zadd elapsed_ns=51918679 first=member-00000000,member-00000001,member-00000002 last=member-00099999,member-00099998,member-00099997
golden sha256=a2592239f6632c9c909dd1842175e8162c352289f8572c41fe861dbc6425f2fc
```

Original saved hyperfine baseline:

```text
cold-range: 1.900 s +/- 0.038 s
one-shot:    85.0 ms +/- 11.0 ms
zadd:        61.9 ms +/- 5.7 ms
```

## Candidate

Built through RCH on worker `vmi1152480` using:

```text
CARGO_TARGET_DIR=target-mn0qm-candidate2-harness cargo build --release --manifest-path artifacts/optimization/frankenredis-mn0qm/pass145/harness/Cargo.toml
```

Direct candidate:

```text
cold-range elapsed_ns=15974913 checksum=245608431828199426492213436369610368752
zadd elapsed_ns=52239826 first=member-00000000,member-00000001,member-00000002 last=member-00099999,member-00099998,member-00099997
golden sha256=a2592239f6632c9c909dd1842175e8162c352289f8572c41fe861dbc6425f2fc
```

## Paired Hyperfine

All paired hyperfine runs used the RCH-built baseline and candidate binaries in
one invocation.

```text
cold-range baseline:  1.9382147425 s +/- 0.0501124705 s
cold-range candidate: 0.1058117551 s +/- 0.0067233639 s
cold-range speedup:   18.32x +/- 1.26x

one-shot baseline:    0.0850703525 s +/- 0.0116804360 s
one-shot candidate:   0.0903062810 s +/- 0.0090897763 s
one-shot guard:       neutral within noise, baseline/candidate ratio 1.06 +/- 0.18

zadd baseline:        0.0624861577 s +/- 0.0056991226 s
zadd candidate:       0.0631940540 s +/- 0.0051554240 s
zadd guard:           neutral within noise, baseline/candidate ratio 1.01 +/- 0.12
```

Direct in-process cold-range speedup:

```text
1857055898 ns / 15974913 ns = 116.25x
```

Score:

```text
Impact 18.32 x Confidence 0.90 / Effort 1.5 = 10.99
```

Kept because Score >= 2.0 and the primary profile-backed target improved by
18.32x in paired hyperfine without measurable write-path regression.

## Isomorphism Proof

Ordering and tie-breaking:

- The authoritative sorted-set order remains `BTreeMap<ScoreMember, ()>`.
- The adaptive treap is constructed from `self.ordered.keys().cloned()` in
  exactly that order.
- `ScoreMember` comparison remains the only ordering contract for score/member
  tie-breaking.
- The treap is only an index over the same keys, and tests compare cold and warm
  `ZRANGE`/`ZREVRANGE`/`WITHSCORES` output.

Floating point:

- The lever performs no score arithmetic.
- Scores are cloned from existing `ScoreMember` keys and returned unchanged.
- NaN/infinite rejection and score canonicalization paths are untouched.

RNG and metadata:

- LFU random sampling remains before access exactly as before.
- `entry.bump_lfu_freq(...)` and `entry.touch(...)` order is unchanged.
- `deep_index_reads` and `rank_tree` are internal metadata with no Redis-visible
  reply surface.

Golden output:

```text
baseline sha256:  a2592239f6632c9c909dd1842175e8162c352289f8572c41fe861dbc6425f2fc
candidate sha256: a2592239f6632c9c909dd1842175e8162c352289f8572c41fe861dbc6425f2fc
```

Checksums:

```text
baseline cold checksum:  245608431828199426492213436369610368752
candidate cold checksum: 245608431828199426492213436369610368752
```

## Validation

Passed:

```text
RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=target-mn0qm-validate CARGO_BUILD_JOBS=1 cargo test -j 1 -p fr-store repeated_deep_zrange_adaptively_warms_rank_tree_mn0qm -- --nocapture
RCH_REQUIRE_REMOTE=1 RCH_WORKER=vmi1149989 rch exec -- env CARGO_TARGET_DIR=target-mn0qm-validate CARGO_BUILD_JOBS=1 cargo test -j 1 -p fr-store zset_index_slice_treap_matches_linear_and_reports_ab_ratio -- --nocapture
RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=target-mn0qm-check CARGO_BUILD_JOBS=1 cargo check -j 1 -p fr-store --all-targets
RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=target-mn0qm-clippy CARGO_BUILD_JOBS=1 cargo clippy -j 1 -p fr-store --all-targets -- -D warnings
cargo fmt -p fr-store --check
git diff --check
ubs crates/fr-store/src/lib.rs
```

UBS note: scan exited 0. It recorded the existing `fr-store` inventory while its
built-in formatting, clippy, cargo check, and test-build subchecks passed.
