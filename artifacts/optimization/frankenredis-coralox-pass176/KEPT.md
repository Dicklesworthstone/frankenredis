# frankenredis-tbmu1 kept: HSET packed hash borrowed overwrite

## Target

- Bead: `frankenredis-tbmu1`
- Parent lane: `frankenredis-ohsk5` / `frankenredis-gu5nf`
- Profile-backed route:
  - Pass 175 current-main P16/C50/n300k sweep selected HSET as the only clear
    Redis-over-FR residual: FrankenRedis `739151.47 ops/sec`, Redis
    `862417.45 ops/sec`, Redis/fr `1.1668x`; p99 FrankenRedis `4291us`,
    Redis `1721us`.
  - Kernel sampling was blocked by `/proc/sys/kernel/perf_event_paranoid=4`;
    `samply` was also blocked by the same policy.
  - Prior HSET parser/direct-output micro-levers were rejected, so this pass
    attacked the deeper packed small-hash store layout primitive.

## Lever

- `HashFieldMap::Packed` borrowed HSET now calls `PackedStrMap::insert_borrowed`.
- Existing-field packed hash overwrites:
  - preserve field position and iteration order,
  - avoid allocating an owned field key,
  - avoid materializing the old value,
  - overwrite the existing record in place when the encoded value length is
    unchanged.
- True new fields still append the same `[klen][k][vlen][v]` record and return
  `true`; promotion checks and hashtable behavior are unchanged.

## Baseline

- Built current baseline with `rch`:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass176-current-target`
  - command: `cargo build --profile release-perf -p fr-server -p fr-bench`
  - worker: `vmi1149989`
- Independent HSET P16/C50/n1M hyperfine:
  - baseline: `1.2597012969857144s +/- 0.08824152952243251s`
  - last run: `944399.03 ops/sec`, p50 `803us`, p95 `1127us`, p99 `1479us`

## Behavior Proof

- Packed-map isomorphism:
  - `PackedStrMap::insert_borrowed(field, value)` was added to the existing
    proptest against `IndexMap`.
  - RCH gate passed:
    `cargo test -p fr-store map_equivalent_to_indexmap -- --nocapture`.
  - The proptest covers inserts, removals, reads, borrowed inserts/overwrites,
    length-changing values, return value (`is_new`), length, and iteration order.
- Raw TCP HSET/HGET/HGETALL golden:
  - input SHA256:
    `07a2d97c8bc906bad830fd87ff1bd2ce407975d3dbcf20153ae4f566ab70f40a`
  - baseline output SHA256:
    `ea34d540dad6c1176557492ae693c76d79fde38df8aebd130df1bdecf029b280`
  - candidate output SHA256:
    `ea34d540dad6c1176557492ae693c76d79fde38df8aebd130df1bdecf029b280`
- Isomorphism checklist:
  - Ordering/tie-breaking: existing-field overwrite keeps the same packed record
    position; true inserts append exactly as before; `HGETALL` order is unchanged.
  - Error semantics: wrongtype and promotion paths remain owned by existing store
    and runtime logic.
  - Floating point: no FP path touched.
  - RNG/LFU: `Store::hset_borrowed` still calls LFU/RNG bookkeeping before the
    map write; the map-only change does not sample RNG.

## Gates

- `rustfmt --edition 2024 --check crates/fr-store/src/packed_set.rs`: passed.
- `cargo check -p fr-store --all-targets` via `rch`: passed with pre-existing
  duplicate `#[test]` warning in `crates/fr-store/src/lib.rs`.
- `cargo test -p fr-store map_equivalent_to_indexmap -- --nocapture` via `rch`:
  passed.
- `cargo build --profile release-perf -p fr-server -p fr-bench` via `rch`:
  passed for baseline and candidate.
- `ubs crates/fr-store/src/packed_set.rs`: exit 0; no critical findings.
- `cargo clippy -p fr-store --all-targets -- -D warnings` via `rch`: blocked by
  pre-existing lint debt:
  - `crates/fr-store/src/lib.rs:37705` duplicate `#[test]`
  - `crates/fr-store/src/lib.rs:37675` collapsible `if`
  - `crates/fr-store/src/lib.rs:37728` needless range loop
  - `crates/fr-store/src/packed_set.rs:2731` manual `div_ceil` in a pre-existing
    benchmark helper
- `cargo fmt -p fr-store -- --check`: blocked by pre-existing formatting drift in
  `crates/fr-store/src/lib.rs`; touched file check passed.

## Re-benchmark

- Paired HSET P16/C50/n1M hyperfine:
  - baseline: `1.2771024027399998s +/- 0.07282719709485148s`
  - candidate: `1.2088866161399998s +/- 0.08897005309457193s`
  - summary: candidate `1.06 +/- 0.10x` faster
  - last-run throughput: baseline `764483.97 ops/sec`, candidate
    `874967.96 ops/sec`
- Confirmation HSET P16/C50/n3M hyperfine:
  - baseline: `3.8005059938714285s +/- 0.22754844721772677s`
  - candidate: `3.3456752352999994s +/- 0.13439416200891594s`
  - summary: candidate `1.14 +/- 0.08x` faster
  - last-run throughput: baseline `871020.35 ops/sec`, candidate
    `932557.63 ops/sec`
  - last-run p99: baseline `1631us`, candidate `1504us`

## Score

- Impact: `1.14`
- Confidence: `3.0` (golden/proptest/check pass, longer confirmation excludes
  zero; short paired run was noisy)
- Effort: `1.5`
- Score: `1.14 * 3.0 / 1.5 = 2.28`

## Decision

- Kept.
- Production source change is one lever in `crates/fr-store/src/packed_set.rs`.
- Evidence retained in this directory.

## Next Route

Re-profile the shifted HSET lane. Avoid repeating packet/direct-output shims.
The next deeper primitive should target either parser arena/region reuse across a
readable batch or another store-layout/key-comparison primitive with fresh
profile support.
