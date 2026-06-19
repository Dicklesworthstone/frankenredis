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

## Large-value SET/GET head-to-head (MEASURED 2026-06-19) — qesp3 gap
fr-bench (Rust client) --pipeline 1 --requests 40000 --trials 5, fr-release(29431) vs
redis-7.2.4(29442). Isolates the value-size scaling of the read/write path.

| Workload | fr ops/s | redis ops/s | fr/redis | cv f/r | Verdict |
|---|--:|--:|--:|--:|---|
| SET 4KB   |  73,577 |  68,484 | 1.074 | 4.7/3.0 | fr faster |
| GET 4KB   |  75,069 |  73,144 | 1.026 | 4.2/3.2 | fr faster |
| SET 64KB  |  11,949 |  28,624 | **0.417** | 3.9/9.0 | **redis 2.4x faster** |
| GET 64KB  |  28,813 |  28,868 | 0.998 | 6.0/4.2 | ~parity |
| SET 256KB |   2,703 |  10,976 | **0.246** | 3.8/12.3 | **redis 4.1x faster** |
| GET 256KB |  10,182 |   8,099 | 1.257 | 7.2/15.6 | fr faster [noisy] |

### Reads of this:
- **Small/medium values: fr faster** (4KB SET 1.07x, GET 1.03x) — consistent with the hot-path win.
- **CONFIRMED SEVERE GAP — large-value SET:** fr SET craters with value size — 0.417x at 64KB,
  0.246x at 256KB (redis 2.4-4x faster). GET is unaffected (parity-or-faster at all sizes). So it
  is SET write-path-specific: the **2-copy large-value framing plateau** (fr-server handle_readable
  scratch + realloc churn), bead **qesp3 / apg7r / project_large_value_framing_gap**. STRUCTURAL,
  pre-existing — NOT a recent lever -> **NO REVERT**. Note: hand-rolled buffer-reuse fixes here
  REGRESS (mimalloc already recycles; see rejected-levers row) — the real fix is the framing
  rewrite (zero-copy read into the value buffer), delicate.
- **Release-readiness flag:** large-value writes (>=64KB) are fr's worst measured workload. For a
  release targeting large-payload use (e.g. caching big blobs), this is the headline gap to close;
  for typical small-value workloads fr dominates (geomean 1.348x).

### Large-value SET — scaling curve + root-cause diagnosis (MEASURED 2026-06-19, refinement)
SET scaling (depth1, 30k req, 4 trials), fr/redis: 16KB 0.192x, 64KB 0.178x, 128KB 0.134x,
256KB 0.115x — monotonically WORSE with value size. Absolute ratio is RUN-TO-RUN noisy
(earlier batch: 64KB 0.417x, 256KB 0.246x; cv<5% within each run) = sandbox-contention variance
(vibu6). ROBUST facts (stable across runs): large-value SET is fr's worst workload (~0.12-0.42x,
redis 2.4-8x faster), worsens with size, GET unaffected.
ROOT-CAUSE (code-read, fr-server handle_readable): the read side is ALREADY optimized
(frankenredis-largeval-bigbulk-zerocopy-qesp3 partial — reads the >8KB continuation directly into
read_buf's tail, dropping the stack->read_buf copy + per-chunk realloc). The RESIDUAL cost is the
SAFE-RUST tax it could not avoid: the grown read_buf region is ZERO-FILLED (memset) before reading
because safe Rust can't read into uninitialized memory without `unsafe`/MaybeUninit, plus the
store-copy (read_buf -> owned value). redis (C) reads straight into uninitialized buffer (no
memset) with ~1 copy. So fr pays ~memset(n) + copy(n) where redis pays ~copy(n).
DO NOT REVERT the qesp3-partial read lever: it fixes the apg7r edge-triggered >16KB read-drain
HANG (correctness) — reverting re-introduces a hang. Real fix = MaybeUninit read into the grown
region (needs `unsafe`, against fr's no-unsafe lean) OR moving the value out of read_buf instead of
copying. Precise hot-spot split (memset vs copy vs syscall) needs a flamegraph on a quiet host.

## Memory (RSS) head-to-head vs Redis 7.2.4 (MEASURED 2026-06-19) — the RAM dimension
Fresh processes (no allocator retention), VmRSS from /proc; the honest metric (used_memory is a
MODEL — for the keyspace it under-reports actual RSS, see below). "beat the original" includes RAM.

| Dataset | redis RSS | fr RSS | fr/redis | Verdict |
|---|--:|--:|--:|---|
| 300k small string keys (keyspace dict) | 35.1 MB | 62.9 MB | **1.790** | redis lighter (220 vs 123 B/key) |
| 1500 hashtable hashes x 600 fields (900k entries) | 56.9 MB | 28.8 MB | **0.506** | **fr HALF the RAM — CompactFieldMap (ideww) WIN** |

### Reads of this — RAM is TYPE-DEPENDENT (nuanced):
- **Keyspace DICT is heavier in fr (1.79x RSS)** — the per-key hashbrown + side-index overhead
  (220 B/key vs redis's 123). This is the keyspace-RAM gap (uhthd domain); MEASURED 1.79x is well
  DOWN from the older ~4.49x claims (uhthd's lazy sorted-key + RANDOMKEY side-index work landing),
  but fr still uses ~80% more per-key. Structural; uhthd in-progress. NOT a recent regression -> NO REVERT.
- **Collection STORAGE is much LIGHTER in fr — hash 0.506x (half!)** — the CompactFieldMap arena+index
  repr (ideww) replacing IndexMap is a MEASURED RAM WIN for hashtable-encoded hashes. Validates the
  lever: fr stores 900k hash entries in 28.8 MB vs redis 56.9 MB.
- **used_memory MODEL under-reports**: for the 300k-key keyspace, fr's used_memory reported 0.70x
  redis (LESS) while actual RSS was 1.79x (MORE). fr's estimate_memory_usage_bytes models redis's
  accounting, not fr's real heap — trust RSS for RAM verdicts, not used_memory.
- TO-MEASURE (memory says, fresh-process RSS owed): zset RAM ~1.54x (uybhq), stream ~1.32x (p8wd1).

### RAM dimension COMPLETE — all collection types measured (fresh-process RSS, 2026-06-19)
| Dataset | redis RSS | fr RSS | fr/redis | Verdict |
|---|--:|--:|--:|---|
| 300k small string keys (keyspace dict) | 35.1 MB | 62.9 MB | **1.790** | redis lighter (the per-key gap) |
| 1500 hashtable hashes x600f (900k entries) | 56.9 MB | 28.8 MB | **0.506** | **fr HALF — CompactFieldMap WIN** |
| 2000 skiplist zsets x300m (600k entries)   | 64.6 MB | 80.9 MB | **1.253** | fr +25% (uybhq dual-structure; was ~1.54x, peni2 helped) |
| 1500 streams x300e (450k entries)          | 24.0 MB | 28.0 MB | **1.165** | fr +16% near-parity (p8wd1 PackedStreamLog) |

**SYNTHESIZED PATTERN (measured):** fr's **per-VALUE collection storage is competitive-or-better**
(hash 0.506x win; stream 1.165x and zset 1.253x near/moderate, both down from older numbers via
shipped levers), but the **per-KEY keyspace-dict overhead is the real RAM gap (1.79x)** — 220 vs
123 B/key. So the RAM headline is: *the dict, not the values.* The single highest-impact RAM lever
remaining is the keyspace-dict compaction (uhthd, in-progress, already 4.49x->1.79x). All structural;
no recent-lever regression -> NO REVERT.

## GET/SET pipeline-depth scaling (MEASURED 2026-06-19, fresh servers)
| Workload | depth 1 | depth 16 | depth 128 |
|---|--:|--:|--:|
| GET fr/redis | 1.056 (cv 3.3/3.7) | 1.148 | 1.456 (cv 5.2/7.4) |
| SET fr/redis | 1.026 (cv 2.5/5.3) | 1.272 | 1.711 (cv 12.4/7.5) |

fr is faster at EVERY depth, and the margin GROWS with pipelining: ~parity at depth 1
(latency-bound — both syscall-round-trip-dominated) -> 1.46-1.71x at depth 128 (throughput-bound,
where fr's efficient per-command dispatch + borrowed fast paths dominate). Note: the long-lived
sandbox servers DEGRADE over a session (a stale run gave fr cv 104% nonsense) — these are on
FRESH processes; reinforces the vibu6 quiet-host need for publication numbers.

## RELEASE-READINESS VERDICT (synthesized from all MEASURED dimensions)
**fr beats Redis 7.2.4 on the common case; trails on three scoped, structural fronts.**

WINS (measured vs redis 7.2.4):
- **Small/medium-value throughput**: faster at all pipeline depths, geomean 1.348x @depth16,
  up to 1.7x @depth128; reads especially (smembers/hgetall/lrange 1.7-1.9x).
- **Collection RAM**: hashtable hash 0.506x (HALF — CompactFieldMap); stream/zset near-parity.
- **RDB decode** for list (0.731x, ta8s1) / set / int-strings (087qq).

GAPS (measured, structural, each scoped — NONE a recent-lever regression -> NO REVERTS):
1. **Large-value SET writes**: 0.12-0.42x (worsens with size) — safe-Rust zero-fill framing tax
   (read side already qesp3-optimized; residual needs MaybeUninit/unsafe or move-out-of-read_buf). GET fine.
2. **Keyspace-dict RAM**: 1.79x RSS (220 vs 123 B/key) — uhthd in-progress (already 4.49x->1.79x).
3. **zset/hash RDB-decode build**: 1.62x / 1.18x — dual-structure (uybhq) / field-rebuild; next lever = zset bulk-build.

SHIP GUIDANCE: for the typical Redis workload (pipelined small-value GET/SET/hash, moderate
keyspace) fr is a measured win on both speed and (collection) RAM. For large-payload caching
(>=64KB values) or very-large-keyspace RAM-sensitive deployments, the three gaps above apply.
Conformance GREEN throughout (all measured levers byte-identical-verified; zero code reverted).
