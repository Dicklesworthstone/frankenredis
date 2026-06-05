# frankenredis-fv18u Rejection Proof

## Target

Profile-backed follow-up to `frankenredis-3gonq` on the RDB multidb encode
harness:

```text
target-cod-fv18u-*/release/fr-persist-rdb-multidb-bench --mode bench --dbs 4096 --entries-per-db 2 --iters 500
```

The prior pass49 profile showed `encode_rdb_internal` as the top RDB hotspot,
with `rdb_encode_length`, `rdb_encode_string`, and `memmove` still visible after
the streaming-CRC and DB-bucketing candidates were rejected.

## Lever Tested

Initialize the RDB output buffer with a conservative precomputed capacity before
emitting bytes:

```rust
Vec::with_capacity(estimate_rdb_capacity(entries, aux, functions))
```

The estimator walked the already-owned `RdbEntry` slice once and overestimated
DB selector overhead to avoid duplicating the exact sort/group planner.

## Baseline

Baseline binary:

```text
target-cod-fv18u-baseline/release/fr-persist-rdb-multidb-bench
```

Standalone baseline hyperfine:

```text
180.4 ms +/- 16.7 ms
```

Paired baseline hyperfine:

```text
170.4 ms +/- 6.0 ms
```

Representative golden RDB:

```text
9c0b4109f8ece11500fd11e9d559c1c70cade75ca87a60ff78b890e9fc627e95
```

## Candidate

Candidate binary:

```text
target-cod-fv18u-candidate/release/fr-persist-rdb-multidb-bench
```

Paired candidate hyperfine:

```text
175.3 ms +/- 8.8 ms
```

Hyperfine summary:

```text
baseline ran 1.03 +/- 0.06 times faster than candidate
```

## Isomorphism

- Ordering preserved: yes. The candidate did not change the existing global
  `(db, key)` stable ordering or any value emission branch.
- Tie-breaking unchanged: yes. No sorted-set, stream, or command ordering rules
  changed.
- Floating-point: unchanged. The estimator only counted bytes and never touched
  sorted-set scores.
- RNG: unchanged. RDB encoding has no RNG side effects.
- Golden output: baseline and candidate RDB files were byte-identical:

```text
9c0b4109f8ece11500fd11e9d559c1c70cade75ca87a60ff78b890e9fc627e95
cmp baseline-golden.rdb candidate-golden.rdb: pass
```

## Validation

```text
cargo fmt --check -p fr-persist
rch exec -- cargo check -p fr-persist --all-targets
```

Both passed after the rejected source hunk was removed.

## Decision

Rejected. The capacity preplanning pass preserved behavior but made the paired
benchmark slower, so it failed the Score >= 2.0 keep gate. The production
`fr-persist` hunk was removed before commit.

## Next Primitive

Stop adding pre-encode walks. The next RDB primitive should remove work from
the hot emitter itself: compact inline length/string emission or a typed RDB
writer that fuses opcode, length, and payload append paths while preserving the
exact byte stream. Target ratio: at least 1.15x on the same multidb harness.
