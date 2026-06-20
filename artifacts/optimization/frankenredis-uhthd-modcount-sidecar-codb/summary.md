# frankenredis-uhthd cod-b sparse modification-count sidecar rejection

Date: 2026-06-20
Agent: CobaltCove / cod-b
Target: keyspace RAM gap versus vendored Redis 7.2.4

## Candidate

Move `Entry.modification_count: u64` out of every keyspace entry into a sparse
`HashMap<StoreKey, u64>` sidecar. Fresh inserts would carry implicit epoch 0,
and only overwritten, removed, or in-place-mutated keys would allocate a sidecar
row. This was the "alien" layout lever: shrink the hot entry and pay metadata
only for keys that actually need WATCH/HLL/memory-cache invalidation epochs.

## Verification

- Baseline build before candidate:
  `AGENT_NAME=CobaltCove rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo build --release -p fr-server -p fr-bench`
- Baseline memory harness:
  `AGENT_NAME=CobaltCove env FR_BENCH_PORT_BASE=44451 python3 scripts/memory_baseline_capture.py /dp/frankenredis/legacy_redis_code/redis/src/redis-server /data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis`
- Candidate focused gates:
  `cargo check -p fr-store --all-targets`,
  `cargo test -p fr-store incrby_existing_key_matches_whole_entry_replacement_side_effects -- --nocapture`,
  `cargo test -p fr-store incr_invalid_integer_leaves_entry_and_side_effects_unchanged -- --nocapture`,
  `cargo test -p fr-store pfcount_multi_key_register_cache_rejects_stale_in_place_string_mutation -- --nocapture`,
  `cargo test -p fr-store value_size_is_capped_by_boxing_sortedset -- --nocapture`.
- Candidate release build:
  `AGENT_NAME=CobaltCove rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo build --release -p fr-server -p fr-bench`
- Candidate memory harness:
  `AGENT_NAME=CobaltCove env FR_BENCH_PORT_BASE=44551 python3 scripts/memory_baseline_capture.py /dp/frankenredis/legacy_redis_code/redis/src/redis-server /data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis`

## Results

Layout proof from the candidate:

```text
after boxing SortedSet+Hash+Set: Value=24 Entry=32 (was 120/168) | unboxed inners: SortedSet=160 Hash=96 Set=96 List=32
```

Baseline fr/Redis memory ratios before the candidate:

| data type | RSS | used_memory |
|---|---:|---:|
| keyspace | 1.267x | 0.805x |
| string_1k | 0.924x | 0.964x |
| list | 1.127x | 0.391x |
| hash | 1.284x | 0.838x |
| set | 1.305x | 0.562x |
| zset | 1.613x | 0.620x |
| stream | 0.950x | 1.057x |

Candidate fr/Redis memory ratios:

| data type | RSS | used_memory |
|---|---:|---:|
| keyspace | 1.459x | 0.805x |
| string_1k | 0.906x | 0.964x |
| list | 1.181x | 0.391x |
| hash | 1.325x | 0.838x |
| set | 1.121x | 0.562x |
| zset | 1.812x | 0.620x |
| stream | 0.983x | 1.096x |

Harness verdict:

```text
FAIL - 1 data-type(s) RAM-regressed vs baseline > 15.0%:
  keyspace: RSS ratio 1.267 -> 1.459 (+15.2% worse)
```

The requested `cargo bench --release` syntax is not accepted by current Cargo
for `cargo bench`; rerunning without `--release` failed before producing data
because RCH rewrote `FR_SERVER_BIN` to a worker target path where the server
binary was not present. This is harness negative evidence, not a performance
claim.

`perf stat -e cycles:u,instructions:u` was blocked by the host kernel:
`perf_event_paranoid=4`.

## Decision

Rejected and reverted before commit. The isolated layout win (`Entry=32`) did
not translate to process RSS; the sidecar dictionary overhead and mutation
epoch churn worsened the target keyspace RSS by 15.2% versus the captured
baseline. Do not retry a standalone sparse modification-count sidecar for
`uhthd`; a future keyspace RAM attempt needs a different metadata/layout
primitive and must beat the memory harness before shipping.

Release-readiness impact: no source shipped, no score improvement. Keyspace RSS
remains a release-readiness gap.
