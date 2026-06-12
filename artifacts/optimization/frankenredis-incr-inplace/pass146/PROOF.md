# frankenredis-1pmno pass146 proof

## Target
- Bead: `frankenredis-1pmno`
- Lever: existing live `INCR` / `INCRBY` string-or-integer keys rewrite the `Entry` in place to `Value::Integer(next)` instead of cloning the key and replacing through `internal_entries_insert`.
- Profile source: current `main` at `b24685c5a`.
- Profile evidence: remote `perf` on `vmi1152480` for INCR/P16 showed `Store::internal_entries_insert` at 5.18%, foldhash key hashing about 5.5%, and `Store::incr` at 2.83%. Dashboard showed INCR at redis/fr 1.42x slower.

## Baseline
- Baseline binary SHA256: `124ebfafab48ccf27d41620d3d8d6d01f4d1ea20a7e9220fee46470f271297a2`
- Local baseline hyperfine:
  - `redis-benchmark -p 23953 -t incr -n 500000 -c 50 -P 16 -r 100000 -q`
  - 658.6 ms +/- 19.7 ms, 15 runs.
- Paired baseline confirmation:
  - 673.7 ms +/- 20.1 ms, 30 runs.

## Candidate
- Candidate binary SHA256: `1b16aad5a6458e588b70e9af320f92653de84e19bba5178de101281a9c7a33e5`
- Paired candidate confirmation:
  - 609.8 ms +/- 23.9 ms, 30 runs.
- Hyperfine summary:
  - candidate ran 1.10x +/- 0.05 faster than baseline.

## Score
- Impact: 10.5% mean latency reduction on the profiled INCR/P16 workload.
- Confidence: 0.80, from 30-run paired hyperfine plus matching golden transcript and targeted invariant tests.
- Effort: 2.0, one localized store lever plus proof/test maintenance.
- Score: 10.5 * 0.80 / 2.0 = 4.2, keep.

## Isomorphism
- Ordering: unchanged. The command stays single-threaded and updates one existing map entry at the same command point.
- Tie-breaking: not applicable; no sorted or unordered iteration output changes.
- Floating point: unchanged; `INCRBYFLOAT` path remains on the prior whole-entry string replacement path.
- RNG: unchanged; the lever does not call `next_rand` or alter random-key indexes.
- Error paths: invalid integer strings, wrong type, and overflow return before mutation, dirty increment, digest mutation, HLL-cache invalidation, or metadata reset.
- Metadata equivalence: unit test compares the new path against explicit old-style `internal_entries_insert(Entry::new(Value::Integer(...)))` replacement for TTL, modification count, LRU/LFU reset, force flags, `int_copy_not_shared`, HLL cache invalidation, volatile bookkeeping, dirty count, digest stale state, and state digest.

## Golden Output
- Fixture: `golden-incr-commands.redis`
- Baseline output SHA256: `4acdf6b733ca8d92c32b75ce310bfe2386ac9d1c0609d7162da4ffd3d1fd69fc`
- Candidate output SHA256: `4acdf6b733ca8d92c32b75ce310bfe2386ac9d1c0609d7162da4ffd3d1fd69fc`

## Gates
- `rch exec -- cargo test -p fr-store incr -- --nocapture`: pass.
- `rch exec -- cargo check -p fr-store --all-targets`: pass.
- `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`: pass.
- `cargo fmt -p fr-store -- --check`: pass.
- `ubs crates/fr-store/src/lib.rs crates/fr-store/tests/metamorphic.rs crates/fr-store/tests/metamorphic_numeric.rs`: nonzero due broad pre-existing full-file findings in `fr-store/src/lib.rs`; UBS internal cargo fmt/clippy/check/test-build sections passed.

## Next Profile Route
- Post-keep quick dashboard (`postkeep-dashboard.txt`, P16/C50, 200k, best-of-2) shows residual slower rows: `spop` 1.15x redis/fr, `incr` 1.13x, `lpush` 1.08x, `set`/`sadd` about 1.03x, `hset` 1.01x. Profile `spop` and the shifted `incr` path before another store lever; expected candidates are expiry lookup, hash/probe layout, parser, or runtime command accounting.
