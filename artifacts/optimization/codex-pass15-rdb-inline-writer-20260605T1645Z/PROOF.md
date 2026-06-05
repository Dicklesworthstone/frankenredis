# frankenredis-kckbe Proof

## Target

Profile-backed RDB multidb encode harness:

```text
target-cod-pass15-*/release/fr-persist-rdb-multidb-bench --mode bench --dbs 4096 --entries-per-db 2 --iters 500
```

Fresh current-main baseline profile:

```text
encode_rdb_internal   58.63%
rdb_encode_string     10.54%
rdb_encode_length      8.95%
memmove                6.01%
stable sort           11.53%
```

## Lever

`rdb_encode_string` now emits `len <= 20` raw strings directly as a one-byte RDB
length followed by payload bytes. This is the exact byte sequence previously
produced by `rdb_encode_length` plus `extend_from_slice`, and upstream skips LZF
for this length range.

The hunk avoids the rejected families from prior passes: DB bucketing, capacity
preplanning, cached sort keys, and standalone streaming CRC.

## Benchmark

Baseline:

```text
standalone: 171.0 ms +/- 3.6 ms
paired3:    179.3 ms +/- 13.6 ms
```

Candidate:

```text
paired3: 155.7 ms +/- 6.7 ms
```

Result:

```text
candidate ran 1.15x +/- 0.10 faster than baseline
Score = 1.15 impact x 0.85 confidence / 0.40 effort = 2.44
```

Earlier confirmation runs also favored the candidate:

```text
paired1: 250.4 ms +/- 19.2 ms -> 217.1 ms +/- 7.2 ms, 1.15x +/- 0.10
paired2: 255.6 ms +/- 17.9 ms -> 209.5 ms +/- 20.7 ms, 1.22x +/- 0.15
```

## Isomorphism

- Ordering preserved: yes. The change does not touch sorting, grouping,
  `SELECTDB`, `RESIZEDB`, expiry, type tags, or value iteration.
- Tie-breaking unchanged: yes. Stable `(db, key)` ordering and duplicate-key
  tie behavior are untouched.
- Floating-point: unchanged. Sorted-set score encoding is untouched.
- RNG: unchanged. RDB encoding has no RNG side effects.
- Golden output: baseline and final candidate RDB bytes are identical.

```text
9c0b4109f8ece11500fd11e9d559c1c70cade75ca87a60ff78b890e9fc627e95
cmp baseline-golden.rdb candidate2-golden.rdb: pass
```

## Validation

```text
cargo fmt --check -p fr-persist
rch exec -- cargo check -p fr-persist --all-targets
rch exec -- cargo clippy -p fr-persist --all-targets -- -D warnings
rch exec -- cargo test -p fr-persist rdb_encode_string -- --nocapture
ubs crates/fr-persist/src/lib.rs
```

UBS remains nonzero because of pre-existing file-wide inventory. Its build
health sections are clean: formatting clean, no clippy warnings/errors, cargo
check clean, and tests build clean.

## Post-Keep Profile

Candidate profile:

```text
encode_rdb_internal   54.32%
stable sort           15.88%
memcmp                 7.81%
rdb_encode_string      7.78%
memmove                7.26%
rdb_encode_length      6.17%
```

The next primitive should target the wider RDB emitter body or sort/memcmp only
with a structurally different plan. Do not repeat capacity preplanning, cached
sort keys, DB bucket planners, or standalone CRC.
