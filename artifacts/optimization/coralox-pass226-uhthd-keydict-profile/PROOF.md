# pass226 evidence: KeyDict main-table-only trial rejected

Bead: `frankenredis-uhthd`

## Current residual after pass225

Fresh-process 1,000,000 persistent string keys, pipeline 256:

- FrankenRedis current (`target-coralox-pass225/release-perf/frankenredis`):
  - RSS delta: `219488 KiB`
  - Bytes per key: `224.755712`
  - Load seconds: `17.916879676`
- Redis 7.2.4 oracle (`legacy_redis_code/redis/src/redis-server`):
  - RSS delta: `84604 KiB`
  - Bytes per key: `86.634496`
  - Load seconds: `16.402577509`
- Residual ratio: `2.59x` bytes/key.

This keeps `frankenredis-uhthd` profile-backed after the Entry-tail shrink.

## Trial lever

Trialed a first structural KeyDict wiring that replaced only
`Store.entries: HashMap<Arc<[u8]>, Entry>` with `KeyDict<Entry>`, while leaving
the existing sorted `ordered_keys`, `random_key_slots`, expiry side dict, and
SCAN/RANDOMKEY behavior intact.

Compile result:

- `CARGO_TARGET_DIR=target-coralox-pass226 cargo check -j1 -p fr-store --all-targets`
  passed after adapting the canonical expiry key lookup to the unchanged
  ordered-key side index.
- `CARGO_TARGET_DIR=target-coralox-pass226 cargo build -j1 -p fr-server --profile release-perf`
  passed.

Benchmark result:

- The 1M-key RSS harness against the KeyDict-main-only candidate ran for more
  than `2m40s` before interruption, versus `17.9s` for current FR.
- The candidate therefore fails the keep gate on throughput before memory can
  matter. No source hunk is retained.

## Decision

Rejected. The half-wired shape pays extra per-node allocation and duplicate key
ownership while preserving the old side indices, so it does not attack enough of
the real structural overhead.

Next route: do not repeat a main-table-only KeyDict swap. The next viable
`uhthd` swing must remove a side-index family as part of the same primitive,
or switch fully to KeyDict-native SCAN/RANDOMKEY with explicit golden SCAN
fixture updates and Redis-oracle proof.

Score: `Impact 0 * Confidence 4 / Effort 2 = 0`.
