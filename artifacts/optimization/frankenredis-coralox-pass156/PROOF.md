# frankenredis-ohsk5.38 proof: direct new-key SADD SetValue construction

## Target

- Parent bead: `frankenredis-ohsk5`
- Child bead: `frankenredis-ohsk5.38`
- Base commit: `09ef332e62723e694abe5a54b68fc84d7fd177aa`
- Fresh P16/C50/n200k dashboard before the lever:
  - `SADD`: Redis `881057 req/s`, FrankenRedis `729927 req/s`, Redis/fr `1.21x`
- CPU `perf` was unavailable and the start-under-strace run did not produce a
  usable syscall summary for this pass, so the accepted target is the
  profile-routed SADD residual plus source-path inspection of missing-key
  `Store::sadd`.

## Lever

One data-structure construction lever in `crates/fr-store/src/lib.rs`:

- Missing-key `Store::sadd` now builds `SetValue` directly instead of first
  building a `GenericSet` and replaying it through `set_entry`.
- The common one-member missing-key path uses a private
  `SetValue::from_single_borrowed` constructor:
  - canonical integer member and `set-max-intset-entries > 0`: `SetValue::Int(vec![n])`
  - otherwise: one-member `GenericSet` via `insert_borrowed`
- Multi-member and empty-member public API paths still use the same
  `SetValue::insert_borrowed` semantics as existing-set SADD.

## Behavior proof

- Replies, side effects, and ordering:
  - added count is unchanged: single new-key SADD returns `1`; multi-member
    duplicate accounting is still from `insert_borrowed`; empty public
    `Store::sadd` still inserts an empty set with added count `0`.
  - integer sets still enumerate in numeric order.
  - generic/listpack sets still enumerate in insertion order.
  - duplicate handling remains set-membership based.
- Encoding:
  - `set-max-intset-entries=0` is preserved by routing single integer members to
    generic storage instead of intset.
  - `refresh_set_encoding_flags` still derives/sticks object encoding from the
    live config thresholds.
- Tie-breaking/floating-point/RNG:
  - no score comparison, floating-point arithmetic, zset tie-breaking, parser
    ordering, hash seed, replication, persistence, or output path changed.
  - the existing missing-key SADD branch does not draw LFU RNG samples; the
    existing-key LFU branch and its `next_rand()` condition are unchanged.
- Golden TCP transcript:
  - baseline:
    `artifacts/optimization/frankenredis-coralox-pass156/golden-baseline-final.out`
  - candidate:
    `artifacts/optimization/frankenredis-coralox-pass156/golden-candidate-final.out`
  - SHA-256 for both:
    `1c2bef06633674267b63b5a5ffb51a08cf39e8bd1b2736d05df05a2767044271`
  - transcript covers integer, generic/listpack, mixed, single-member, duplicate
    SADD, `SCARD`, `SMEMBERS`, and `OBJECT ENCODING`.

## Benchmarks

Saved release binaries:

- baseline: `frankenredis-baseline-09ef332e`
- candidate: `frankenredis-candidate-direct-setvalue-single`

Final path-matched new-key benchmark:

```text
legacy_redis_code/redis/src/redis-benchmark \
  -p <port> -r 100000000 --seed <seed> -n 200000 -c 50 -P 16 \
  --csv SADD saddkey:__rand_int__ member
```

Alternating order, fixed paired seeds, 8 samples per binary:

- baseline geomean: `326438.47 req/s`
- candidate geomean: `339283.85 req/s`
- delta: `+3.94%`
- paired ratio geomean: `1.0394`
- wins: `7/8`
- median p95 latency: `3.207ms -> 2.919ms`

Hyperfine wall-time artifact for the same new-key command shape:

- artifact: `hyperfine_newkey_sadd_p16_c50_n200k_final.json`
- baseline: `669.7ms +/- 29.8ms`
- candidate: `638.3ms +/- 32.0ms`
- summary: candidate `1.05x +/- 0.07` faster

The stock Redis `SADD` dashboard was retained only as routing context because
Redis' built-in SADD benchmark uses one fixed key and random members; it mostly
exercises existing-set insertion, while this lever targets missing-key SADD.

## Gates

- `cargo fmt -p fr-store -- --check`: passed
- `sha256sum -c golden-sha256-final.txt`: passed
- `rch exec -- env CARGO_TARGET_DIR=/data/tmp/coralox-pass156-check2-target cargo check -p fr-store --all-targets`: passed remotely on `vmi1227854`
- `rch exec -- env CARGO_TARGET_DIR=/data/tmp/coralox-pass156-test2-target cargo test -p fr-store sadd -- --nocapture`: passed remotely on `vmi1152480`
- `rch exec -- env CARGO_TARGET_DIR=/data/tmp/coralox-pass156-clippy2-target cargo clippy -p fr-store --all-targets -- -D warnings`: passed remotely on `vmi1149989`
- `ubs crates/fr-store/src/lib.rs`: exit `1` from pre-existing file-wide
  findings; no reported finding targets the changed SADD constructor or
  missing-key branch.

## Decision

Keep.

Score: Impact `1.4` x Confidence `2.0` / Effort `1.0` = `2.8`.
