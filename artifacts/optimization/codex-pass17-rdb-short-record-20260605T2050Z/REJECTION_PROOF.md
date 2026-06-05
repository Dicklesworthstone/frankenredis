# Pass 17 Rejection Proof: fused short RDB string records

Bead: `frankenredis-0iabn`

## Target

Post-pass16 profile on the RDB multidb harness showed:

- `encode_rdb_internal`: 54.32%
- stable sort: 15.88%
- `__memcmp_avx2_movbe`: 7.81%
- `rdb_encode_string`: 7.78%
- `memmove`: 7.26%
- `rdb_encode_length`: 6.17%

The tested one-lever change fused `RDB_TYPE_STRING`, short raw key, and short
raw value emission for string records whose key and value lengths are both
`<= 20`, falling back to the existing `rdb_encode_string` path otherwise.

## Baseline

Built with:

```bash
rch exec -- env CARGO_TARGET_DIR=target-cod-pass17-baseline cargo build --release --manifest-path artifacts/optimization/crimsonfalcon-perf-20260602/fr-persist-rdb-multidb/Cargo.toml
```

Standalone baseline:

```text
Time (mean +/- sigma): 153.7 ms +/- 9.2 ms
```

Golden SHA-256:

```text
9c0b4109f8ece11500fd11e9d559c1c70cade75ca87a60ff78b890e9fc627e95
```

## Candidate

Built with:

```bash
rch exec -- env CARGO_TARGET_DIR=target-cod-pass17-candidate cargo build --release --manifest-path artifacts/optimization/crimsonfalcon-perf-20260602/fr-persist-rdb-multidb/Cargo.toml
```

Behavior proof while the candidate was applied:

```text
candidate golden SHA-256: 9c0b4109f8ece11500fd11e9d559c1c70cade75ca87a60ff78b890e9fc627e95
baseline/candidate cmp: identical
```

Isomorphism:

- RDB byte stream was identical by SHA-256 and `cmp`.
- Existing global `(db, key)` ordering and duplicate-key stable tie behavior
  were not changed.
- Type tags, short length encodings, LZF cutoff, expiry, stream payloads,
  floating-point behavior, and RNG behavior were not changed.

## Paired Benchmark

Command:

```bash
hyperfine --warmup 5 --runs 20 --export-json artifacts/optimization/codex-pass17-rdb-short-record-20260605T2050Z/paired-hyperfine-iters500.json 'target-cod-pass17-baseline/release/fr-persist-rdb-multidb-bench --mode bench --dbs 4096 --entries-per-db 2 --iters 500' 'target-cod-pass17-candidate/release/fr-persist-rdb-multidb-bench --mode bench --dbs 4096 --entries-per-db 2 --iters 500'
```

Results:

```text
baseline:  158.0 ms +/- 5.4 ms
candidate: 152.3 ms +/- 9.2 ms
ratio:     candidate 1.04x +/- 0.07 faster
```

Score gate:

```text
1.78 = 1.04 impact x 0.60 confidence / 0.35 effort
```

## Decision

Rejected. The behavior proof passed, but the effect is small and noisy and the
Score is below the `>= 2.0` keep threshold. The production source hunk was
removed before commit.

## Next Primitive

Stop micro-tuning short-string emission. Re-profile and attack a structurally
different RDB primitive: a caller-certified sorted-entry RDB emission path that
bypasses the global stable comparison sort only when input order can be proven,
with the existing stable `(db, key)` sort retained as the fallback. Target at
least `1.15x` on the multidb harness while preserving the golden RDB SHA.
