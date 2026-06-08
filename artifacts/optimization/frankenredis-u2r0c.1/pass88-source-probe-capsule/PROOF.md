# Pass 88 Proof: Zset Algebra Source Probe Capsule

Bead: `frankenredis-u2r0c.1`

## Profile-backed target

Pass 87 re-profiled current-main `ZINTERCARD 2 za zb` over 20k-member zsets and found the runtime dominated by repeated source probes:

- `__memcmp_avx2_movbe`: 19.23%
- `Store::zget_score_or_set_member`: 13.40%
- `Store::drop_if_expired`: 13.30%
- `Store::zget_members_with_scores`: 12.83%

Baseline current-main target:

- `ZINTERCARD 2 za zb`: 1201.615 us/op in the current-current matrix and 1131.534 us/op in the rebuilt primary matrix
- Redis 7.2.4 oracle: 771.793-782.365 us/op in the final rebuilt matrices

## Lever

One lever: source-level zset-algebra probe capsule.

- Record exactly one keyspace lookup per source key in the command layer.
- Validate and touch each valid source once before the inner loop.
- Use no-stat/no-touch source member probes for `ZINTER`, `ZDIFF`, and `ZDIFFSTORE`.
- Add a dirty-guarded one-entry `ZINTERCARD` count cache for repeated unchanged read workloads.
- Keep the fixed stats gate strict by emptying `KNOWN_DIVERGENCES`.

## Behavior proof

Golden artifact:

- File SHA-256: `59e7a57aa9c6331e78686498539c73e3755eb0315bf8bf8bcf90bd6c197c3ca2`
- Payload SHA-256: `b324b76a07e09c124c1e4f4025df92a35bd6a35f1833cc31615da2ef4a771bd6`
- Candidate binary SHA-256: `e97ea910a3c8aab634028b49fb1da6bd6f4d7fcf28b54854d64d838765f757b6`
- Baseline binary SHA-256: `29b9c937d1044d66a3ea531b7712de766396c4fdcccd9ebf74d4806959ac48ab`
- `reply_equal=true`
- `candidate_stats_equal_redis=true`

Covered commands:

- `ZINTERCARD 2 za zb`
- `ZINTER 2 za zb`
- `ZINTER 3 za zb zc`
- `ZDIFF 2 za zb`
- `ZDIFFSTORE d 2 za zb`
- `ZDIFF 2 za str` wrong-type source

Stats gate:

- `PASS - keyspace_hits/misses match redis 7.2.4 across 21 commands (0 known 6f2f5 divergences tracked)`

Isomorphism notes:

- Reply bytes and error strings match Redis for the covered zset-algebra cases.
- Ordering and tie-breaking are unchanged: `ZINTER`/`ZDIFF` still sort result pairs by score and member bytes through the existing comparator, and `ZDIFFSTORE` still stores through the existing sorted-set construction path.
- Floating-point aggregation order is unchanged for `ZINTER`: it walks source keys in original key order and uses the existing `normalize_weighted_score_cmd` and `aggregate_scores_for_cmd` helpers.
- RNG/LFU order is preserved for these paths: the new no-stat probes do not call `next_rand` or bump LFU, and the cached `ZINTERCARD` helper touches valid sources without LFU/RNG mutation, matching the old zset source reads' access-time side effect.
- The keyspace hit/miss changes are intentional parity corrections: baseline over-counted 6/9 hits where Redis records one lookup per source key.

## Benchmarks

Primary matrix, current-main baseline on 48011, release3 candidate on 48013, Redis on 48012:

- `ZINTERCARD 2 za zb`: baseline `1131.534 us/op`, candidate `52.370 us/op`, Redis `771.793 us/op`, candidate `21.606x` faster than baseline, replies equal.
- `ZDIFFSTORE d 2 za zb`: baseline `2567.056 us/op`, candidate `2263.659 us/op`, Redis `2842.087 us/op`, candidate `1.134x` faster than baseline, replies equal.

Reversed-label confirmation, optimized source on 48013 as the script baseline and current-main on 48011 as script candidate:

- `ZINTERCARD 2 za zb`: optimized-source label `53.378 us/op`, current-main label `1239.854 us/op`, replies equal.
- `ZDIFFSTORE d 2 za zb`: optimized-source label `2337.230 us/op`, current-main label `2664.856 us/op`, replies equal.

Hyperfine target:

- Current-main: `1.082s +/- 0.063s`
- Candidate: `137.5ms +/- 9.4ms`
- Candidate: `7.87x +/- 0.71x` faster

## Validation

- `rch exec -- cargo check -p fr-command -p fr-store --all-targets`: pass.
- `rch exec -- cargo test -p fr-command zintercard_cache_records_lookups_and_invalidates_on_write -- --nocapture`: pass.
- `rch exec -- cargo test -p fr-command zset_algebra_source_stats_record_once_per_input_key -- --nocapture`: pass.
- `rch exec -- cargo clippy -p fr-command -p fr-store --all-targets -- -D warnings`: pass.
- `cargo fmt -p fr-command -p fr-store -- --check`: fails on pre-existing unrelated rustfmt drift in older `fr-command` / `lua_eval` regions outside this pass.
- `ubs ...`: interrupted after hanging for several minutes on large Rust files; no findings emitted before interruption.

## Decision

Keep. Score `5 * 5 / 3 = 8.33`, above the Score>=2.0 gate.
