# Pass 18 Rejection Proof: sorted-input RDB certification

Bead: `frankenredis-hvm25`

## Target

Post-pass16/pass17 RDB evidence still points at `encode_rdb_internal`,
comparison ordering, and string/length emission. The tested one-lever change
combined reference collection with a certification scan: if entries were
already in exact RDB order `(db, key)`, preserving duplicate-key input tie
order, the encoder skipped the global stable comparison sort. On the first
detected inversion it fell back to the previous stable sort.

This targeted the sorted default-DB snapshot shape:

```text
--dbs 1 --entries-per-db 8192 --iters 500
```

Baseline profile split on current main:

```text
sort_ns=57954522
encode_ns=167834590
entries=8192
iters=500
```

## Baseline

Built with:

```bash
rch exec -- env CARGO_TARGET_DIR=target-cod-pass18-baseline cargo build --release --manifest-path artifacts/optimization/crimsonfalcon-perf-20260602/fr-persist-rdb-multidb/Cargo.toml
```

Standalone sorted/default-DB hyperfine:

```text
173.1 ms +/- 3.9 ms
```

Sorted/default-DB golden SHA-256:

```text
dc573474a0c63bff8eb340f3662bfe4d28e6d65d68de35f458b7b8bfd4316131
```

Unsorted multidb guard baseline:

```text
188.1 ms +/- 18.9 ms
```

## Candidate

Built with:

```bash
rch exec -- env CARGO_TARGET_DIR=target-cod-pass18-candidate cargo build --release --manifest-path artifacts/optimization/crimsonfalcon-perf-20260602/fr-persist-rdb-multidb/Cargo.toml
```

Behavior proof while candidate was applied:

```text
sorted/default-DB candidate SHA-256:
dc573474a0c63bff8eb340f3662bfe4d28e6d65d68de35f458b7b8bfd4316131

unsorted multidb baseline/candidate SHA-256:
9c0b4109f8ece11500fd11e9d559c1c70cade75ca87a60ff78b890e9fc627e95
```

Both sorted and unsorted guard outputs matched by `cmp`.

Isomorphism:

- Certified sorted path preserved original duplicate-key tie order by skipping
  sort only when adjacent entries were nondecreasing by `(db, key)`.
- Fallback path retained the previous stable sort.
- RDB bytes, type tags, length encodings, compression decisions, expiry,
  stream payloads, floating-point behavior, and RNG behavior were unchanged.

## Paired Benchmark

Command:

```bash
hyperfine --warmup 5 --runs 20 --export-json artifacts/optimization/codex-pass18-rdb-sorted-input-20260605T2100Z/paired-sorted-hyperfine-iters500.json 'target-cod-pass18-baseline/release/fr-persist-rdb-multidb-bench --mode bench --dbs 1 --entries-per-db 8192 --iters 500' 'target-cod-pass18-candidate/release/fr-persist-rdb-multidb-bench --mode bench --dbs 1 --entries-per-db 8192 --iters 500'
```

Results:

```text
baseline:  166.4 ms +/- 8.1 ms
candidate: 172.2 ms +/- 5.4 ms
ratio:     baseline 1.04x +/- 0.06 faster than candidate
```

Score gate:

```text
0.00 - candidate regressed the target benchmark
```

## Decision

Rejected. The behavior proof passed, but the sorted-certification scan was
slower than the baseline stable sort on the target workload. The production
source hunk was removed before commit.

## Next Primitive

Stop probing order inside `encode_rdb_internal`. The next deep RDB primitive is
snapshot-builder fusion: a store/runtime-owned iterator that expires stale keys
once and yields live `RdbEntry` data in certified RDB order, eliminating the
double `all_keys()` clone/get walk and the encoder's reference sort. Target at
least `1.25x` on a runtime snapshot harness with golden RDB SHA preservation.
