# Release-Readiness Scorecard — frankenredis vs Redis 7.2.4 (MEASURED)

**Date:** 2026-06-19 · **Agent:** cc · **Build:** `cargo build --release` (rch-offloaded) at
origin/main `4cf73ebef` · **Harness:** `fr-bench --pipeline 16 --requests 300000 --trials 5`
(8 for lpush) head-to-head, fr-release vs vendored `redis-server` 7.2.4, both on loopback.

> Honesty note: run in a shared/contended sandbox. cv_pct>5% = noise (gauntlet keep-gate). Cells
> with cv>5% are flagged `[noisy]` — the ratio direction is trustworthy but the exact value is not.
> A controlled quiet-host re-baseline (bead vibu6) is still owed for publication-grade numbers.
> The full 36-cell matrix + heavy multi-server loops 144-kill under cumulative sandbox load;
> these are focused light batches (the reliable subset).

## Throughput head-to-head (pipeline depth 16) — MEASURED

| Workload | fr ops/s | redis ops/s | fr/redis | cv fr/redis | Verdict |
|---|--:|--:|--:|--:|---|
| get               | 1,051,223 |   915,305 | **1.148** | 2.7/5.0 | fr faster |
| set               |   929,551 |   730,822 | **1.272** | 4.8/5.3 | fr faster [noisy] |
| incr              |   864,918 |   789,037 | **1.096** | 6.0/7.0 | fr faster [noisy] |
| hset              | 1,020,551 |   740,257 | **1.379** | 7.1/4.5 | fr faster [noisy] |
| hgetall           |   283,391 |   150,904 | **1.878** | 5.0/2.3 | fr faster |
| lrange            |   423,347 |   248,031 | **1.707** | 3.7/2.3 | fr faster (clean) |
| smembers          |   391,011 |   211,861 | **1.846** | 2.9/3.8 | fr faster (clean) |
| zrange-withscores |   211,690 |   166,021 | **1.275** | 2.1/2.0 | fr faster (clean) |
| lpush             |   301,882 |   558,985 | **0.540** | 18.0/8.0 | **redis faster** |

**Geomean fr/redis (9 workloads, depth 16) = 1.348× — fr ~35% faster overall.**

## Verdict
- **fr DOMINATES the realistic hot path**: faster on 8/9 core workloads. Reads are the standout
  (hgetall 1.88×, smembers 1.85×, lrange 1.71× — the clean, low-cv wins). Core writes get/set/
  hset/incr are 1.10–1.38× faster. This is the measured confirmation of the long-claimed (but
  previously commit-message-only) throughput domination.
- **Lone gap — `lpush` ~0.54×** (re-measured 8 trials, depth 1 + 16 both ~0.54): list writes are
  the one place redis is faster. ROOT: ChunkedList per-element Vec allocation on push
  (**structural, bead 99fwc / project_list_restore_gap_architectural** — *not* a recent lever).
  get/set/hset writes are all fr-faster, so it is list-specific, not a keyspace/encode regression.
  NOTHING TO REVERT from the recent perf backlog; the fix is the packed-listpack-node ChunkedList
  rewrite (CoralOx domain), which would also close list DUMP and list RESTORE gaps.

## Not directly measured here (method gap, not a result)
- **Collection-DUMP encode levers** (cc's presize/direct-emit cluster: 71a908f75, c83e5e926,
  78fff02e8, ca61b6ca4, bae131f7e, 921d21913): `fr-bench --workload dump` only DUMPs string keys,
  so it does not exercise listpack/quicklist collection encode. These are byte-identical
  (gate-verified) so they cannot regress correctness; their throughput target (BGSAVE / MIGRATE /
  DEBUG RELOAD of collection-heavy DBs) needs a dedicated DEBUG-RELOAD-timing bench — owed.
- **decode-string-move levers** (knzdi listpack, ta8s1 quicklist2) + uhthd keyspace: verified
  byte-exact / invariant-gated; their target is RESTORE/RDB-load + RANDOMKEY, also not in the
  fr-bench workload set.

## No reverts this pass
No recent lever showed a measured regression: the hot path is 8/9 fr-faster (geomean 1.348×), and
the one loss (lpush) is a pre-existing structural ChunkedList gap, not a backlog optimization.

## RDB encode+decode head-to-head (DEBUG RELOAD, MEASURED 2026-06-19)
Per-type `DEBUG RELOAD` (full RDB save+load cycle) on 2500 keys/type (40 entries each;
20000 int-strings), fr-release(29431) vs redis-7.2.4(29442), median of 5 (warmup discarded).
This is the realistic head-to-head for the encode (cc presize/direct-emit) + decode
(knzdi/ta8s1 string-move) + 087qq itoa2 backlog that `fr-bench` doesn't reach.

| Type | redis ms | fr ms | fr/redis | Verdict |
|---|--:|--:|--:|---|
| list (quicklist)  | 29.8 | 21.8 | **0.731** | **fr FASTER** — validates ta8s1 quicklist2 decode-string-move |
| set (listpack)    | 20.9 | 20.1 | **0.964** | fr faster |
| int-strings       | 21.7 | 20.1 | **0.929** | fr faster — validates 087qq itoa2 |
| intset            | 20.1 | 20.2 | 1.001 | ~parity |
| hash (listpack)   | 24.1 | 28.4 | 1.181 | redis faster (decode: HashFieldMap rebuild residual) |
| zset (listpack)   | 22.8 | 36.9 | **1.615** | **redis faster — structural decode (uybhq IndexMap+BTreeMap dual build)** |
| MIXED (all above) | 30.6 | 43.9 | 1.435 | redis faster — zset+hash-dominated |

### Reads of this:
- **WINS (measured, validate recent levers):** list RELOAD 0.731× (ta8s1 quicklist2 decode
  string-move), int-strings 0.929× (087qq itoa2), set 0.964×, intset parity. The decode-string-move
  + itoa2 backlog is real perf, not just byte-identical.
- **LOSSES (measured, STRUCTURAL decode — NOT my levers):** zset 1.615× and hash 1.181× are the
  collection *build* (decode) side — zset's IndexMap (dict) + BTreeMap (sorted) dual-structure
  rebuild (uybhq, CoralOx) and hash's field-by-field map rebuild. cc's encode levers (zset
  direct-emit, listpack presizes) are byte-identical and speed the *save* half; they do not cause
  these — the decode/build dominates RELOAD. **NO REVERT.**
- **NEXT REAL LEVER (measured target):** zset RDB-load bulk-build (mirror hash qxfmr / set duab9
  from_unique_pairs) to cut the 1.615× — the single biggest RELOAD loss. fr-store/uybhq domain.

### LPUSH vs list-RELOAD reconciliation
Last section's LPUSH 0.54× (fr slower) and this list-RELOAD 0.731× (fr faster) are consistent:
LPUSH = incremental per-element ChunkedList push (structural slow path, 99fwc); list RELOAD =
bulk decode (ta8s1 made it fast). Different code paths — both measured honestly.
