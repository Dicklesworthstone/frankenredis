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
  original fr-bench workload set. Later focused Criterion RESTORE rejected `ta8s1`; see below.

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
| list (quicklist)  | 29.8 | 21.8 | **0.731** | **fr FASTER** in broad RELOAD; not sufficient `ta8s1` proof |
| set (listpack)    | 20.9 | 20.1 | **0.964** | fr faster |
| int-strings       | 21.7 | 20.1 | **0.929** | fr faster — validates 087qq itoa2 |
| intset            | 20.1 | 20.2 | 1.001 | ~parity |
| hash (listpack)   | 24.1 | 28.4 | 1.181 | redis faster (decode: HashFieldMap rebuild residual) |
| zset (listpack)   | 22.8 | 36.9 | **1.615** | **redis faster — structural decode (uybhq IndexMap+BTreeMap dual build)** |
| MIXED (all above) | 30.6 | 43.9 | 1.435 | redis faster — zset+hash-dominated |

### Reads of this:
- **WINS (measured, validate recent levers):** list RELOAD 0.731× in the broad save+load path,
  int-strings 0.929× (087qq itoa2), set 0.964×, intset parity. The focused `ta8s1` RESTORE
  gate below supersedes the broad list read for that specific lever.
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
bulk save+load. Different code paths — both measured honestly. The focused `ta8s1` RESTORE
gate below rejected the owned-entry-move hunk despite this broad list result.

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

**Latest uhthd status (cod-b, 2026-06-20):** boxed canonical keys remain a keep from the previous
pass, but the follow-on inline-small `StoreKey` enum was rejected and reverted. Direct A/B regressed
scale-200k keyspace RSS from **1.169x** Redis to **1.465x** Redis and worsened six of seven absolute
FrankenRedis RSS cells. The rebuilt reverted control sample is **1.246x** Redis on keyspace RSS and
**2 wins / 5 losses / 0 neutral** across memory cells, so keyspace RAM remains open. Raw bundle:
`artifacts/optimization/frankenredis-uhthd-smallkey/20260620T0001Z/`.

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
- **RDB decode** for broad list reload (0.731x), set, and int-strings (087qq).

GAPS (measured, structural, each scoped — NONE a recent-lever regression -> NO REVERTS):
1. **Large-value SET writes**: 0.12-0.42x (worsens with size) — safe-Rust zero-fill framing tax
   (read side already qesp3-optimized; residual needs MaybeUninit/unsafe or move-out-of-read_buf). GET fine.
2. **Keyspace-dict RAM**: latest reverted-control harness 1.25x RSS (prior boxed-key gate 1.35x,
   prior 300k readiness table 1.79x) — uhthd in-progress; boxed canonical keys are a measured
   keep, inline-small key wrapping is a measured rejection, and Redis is still lighter.
3. **zset/hash RDB-decode build**: 1.62x / 1.18x — dual-structure (uybhq) / field-rebuild; next lever = zset bulk-build.

SHIP GUIDANCE: for the typical Redis workload (pipelined small-value GET/SET/hash, moderate
keyspace) fr is a measured win on both speed and (collection) RAM. For large-payload caching
(>=64KB values) or very-large-keyspace RAM-sensitive deployments, the three gaps above apply.
Conformance GREEN throughout; measured no-ship candidates and reverts are called out below.

## Cod-a mixed set-algebra retain candidate (MEASURED 2026-06-19)

Criterion harness added in `fr-bench`: `cargo bench -p fr-bench --bench set_algebra_vs_redis
-- --noplot`, release `frankenredis` rch-built under
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`, oracle Redis 7.2.4 at
`/data/projects/frankenredis/legacy_redis_code/redis/src/redis-server`.

| Workload | Redis cmds/s | fr cmds/s after revert | fr/redis | Verdict |
|---|--:|--:|--:|---|
| SINTERSTORE mixed intset/generic | 18,525 | 7,960 | 0.430 | Redis faster; retain hunk rejected |
| SDIFFSTORE mixed intset/generic | 20,562 | 8,053 | 0.392 | Redis faster; retain hunk rejected |
| SUNIONSTORE mixed | 1,903 | 2,298 | 1.208 | fr faster; unrelated existing union-path win |

Decision: `frankenredis-gu5nf.32` does **not** raise release readiness. The candidate
stack-borrowed intset retain bytes showed no keep signal on `SINTERSTORE`, regressed the
fr baseline on `SDIFFSTORE`, and has been reverted. Residual set-algebra risk is scoped:
mixed intset/generic intersection and difference remain slower than Redis, while union is
already faster. Retry only from a fresh profile naming `SetValue::retain` or mixed
set-algebra allocation as a top hotspot.

## Cod-b ZRANGE WITHSCORES score direct-encode (MEASURED 2026-06-19)

Harness: `fr-bench --workload zrange-withscores`. Setup preloads each key with 64
integer-scored sorted-set members, then the timed operation is
`ZRANGE key 0 -1 WITHSCORES`, isolating the `frankenredis-n2u1g` direct score
emit path. Release-perf binaries used `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`;
raw artifacts live in
`artifacts/optimization/frankenredis-n2u1g/verify_zrange_withscores_20260619T0515Z/`.

| Depth | Redis ops/s | fr ops/s | fr/redis | cv redis/fr | p99 redis/fr us | Verdict |
|---:|--:|--:|--:|--:|--:|---|
| p1 | 65,524 | 71,038 | 1.084 | 5.94/2.58 | 99/83 | fr faster [noisy] |
| p16 | 176,576 | 226,505 | 1.283 | 3.67/1.43 | 486/307 | fr faster clean |
| p128 | 188,686 | 259,932 | 1.378 | 0.71/1.54 | 3937/2401 | fr faster clean |

Win/loss/neutral: **3/0/0**. Decision: keep `frankenredis-n2u1g`; no revert.
Conformance guard: `zset_score_emit_differ.py` passed byte-exact vs Redis 7.2.4 for
ZSCORE/ZMSCORE/ZINCRBY/ZADD-INCR/WITHSCORES/ZPOPMIN/ZPOPMAX under RESP2 and RESP3.

## Cod-a integer GET materialization (MEASURED 2026-06-19)

Harness added in `fr-bench`: `--workload integer-get`. Setup uses `INCRBY` to prefill
integer-encoded string values, then the timed operation is `GET`, isolating the
`frankenredis-087qq` `Value::Integer` materialization path. Release binaries were rch-built
under `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`; raw artifacts live in
`artifacts/optimization/frankenredis-087qq/verify_integer_get_20260619T0505Z/`.

| Workload | Redis ops/s | fr ops/s | fr/redis | cv fr/redis | Verdict |
|---|--:|--:|--:|--:|---|
| GET p1 | 83,375 | 93,111 | 1.117 | 5.35/6.13 | fr faster |
| GET p16 | 939,014 | 1,162,525 | 1.238 | 6.37/5.25 | fr faster |
| GET p128 | 2,313,189 | 3,660,514 | 1.583 | 5.99/8.97 | fr faster |
| SET p1 | 88,728 | 99,203 | 1.118 | 4.26/3.65 | fr faster |
| SET p16 | 938,451 | 960,631 | 1.024 | 13.64/10.25 | fr faster [noisy] |
| SET p128 | 1,896,334 | 2,748,513 | 1.449 | 19.01/13.15 | fr faster [noisy] |
| integer-get p1 | 96,367 | 97,091 | 1.008 | 6.36/2.53 | fr faster [noisy] |
| integer-get p16 | 774,653 | 848,026 | 1.095 | 8.24/8.30 | fr faster [noisy] |
| integer-get p128 | 1,769,645 | 2,393,822 | 1.353 | 19.73/8.63 | fr faster [noisy] |

Win/loss/neutral: **9/0/0** cells overall; target `integer-get`: **3/0/0**. Decision:
keep `frankenredis-087qq`. The small p1 margin is noisy but positive, and the pipelined integer
GET cells are clearly fr-faster. No revert.

Validation: focused `fr-bench` fmt/clippy/tests passed, release binaries were rch-built, and the
full workspace gates are green after resolving closeout-only gate debt:
`cargo check --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --check`, and refreshed `cargo test -p fr-conformance -- --nocapture` all passed
(`rch` for the build/check/clippy/conformance gates).

## Cod-a quicklist2 PACKED RESTORE decode move (MEASURED 2026-06-19)

Criterion harness added in `fr-bench`: `cargo bench -p fr-bench --bench
restore_quicklist_vs_redis`, pinned rch worker `vmi1149989`, oracle Redis 7.2.4 at
`/data/projects/frankenredis/legacy_redis_code/redis/src/redis-server`. The harness uses a
Redis-generated type-18 QUICKLIST_2 `DUMP` payload for a 96-member list with 40-byte members, then
times 8-command `RESTORE ... REPLACE` pipelines.

| Workload | Redis cmds/s | fr candidate cmds/s | fr/redis throughput | fr/redis time | Verdict |
|---|--:|--:|--:|--:|---|
| QUICKLIST_2 PACKED RESTORE | 236,900 | 87,777 | 0.371 | 2.699 | Redis faster; `ta8s1` rejected |

Decision: reject `frankenredis-ta8s1` and revert the production hunk back to `entry.to_bytes()`.
The earlier broad DEBUG RELOAD list win was not specific enough to keep this owned-entry-move
decode lever. Release-readiness impact is negative for this focused RESTORE path until a deeper
bulk-list decode/build profile finds a different lever.

## Cod-a quicklist2 RESTORE REPLACE slot reuse (MEASURED 2026-06-19)

Criterion harness: `cargo bench -p fr-bench --bench restore_quicklist_vs_redis -- --noplot`.
Release binaries were rch-built with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`; the timing harness used a
separate local target dir and Redis 7.2.4 from
`/data/projects/frankenredis/legacy_redis_code/redis/src/redis-server`.

| Workload | Redis elems/s | fr elems/s | fr/redis | fr candidate/no-candidate | Verdict |
|---|--:|--:|--:|--:|---|
| QUICKLIST_2 PACKED RESTORE no-candidate | 112,860 | 49,455 | 0.438 | baseline | baseline |
| QUICKLIST_2 PACKED RESTORE in-place REPLACE | 117,310 | 52,584 | 0.448 | 1.063 | keep; Redis still faster |

Decision: keep `frankenredis-tnv37` production hunk. It avoids the remove/reinsert cycle for
`RESTORE ... REPLACE` while explicitly clearing stale hash-field TTL and stream sidecar state.
Release-readiness impact is mixed: a measured +6.33% same-harness win, but the workload remains a
Redis-relative loss at 0.448x. This pass also rejected listpack-count preallocation as a regression.

Validation: focused `cargo test -p fr-store restore_replace -- --nocapture` passed via rch,
including hash-field TTL and stream-consumer-group replacement regressions. Full workspace gates are
tracked with the closeout for this commit.

## Cod-b keyed-write parser backlog (MEASURED 2026-06-19)

Criterion harness added in `fr-bench`: `cargo bench -p fr-bench --bench keyed_write_vs_redis
-- --noplot`, release `frankenredis` rch-built under
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`, oracle Redis 7.2.4 at
`/dp/frankenredis/legacy_redis_code/redis/src/redis-server`.

| Workload | Redis cmds/s | fr cmds/s | fr/redis | fr current/pre-series | Verdict |
|---|--:|--:|--:|--:|---|
| LPUSH 5 values  | 652,752 | 266,685 | 0.409 | 1.019 | Redis faster; parser not enough |
| LPUSH 8 values  | 574,203 | 200,729 | 0.350 | 1.036 | Redis faster; parser not enough |
| LPUSH 12 values | 433,576 | 143,680 | 0.331 | 1.095 | Redis faster; modest A/B win |
| LPUSH 16 values | 395,036 | 107,754 | 0.273 | 1.039 | Redis faster; parser not enough |
| RPUSH 5 values  | 812,741 | 650,096 | 0.800 | 1.173 | Keep: A/B win |
| RPUSH 8 values  | 727,872 | 583,571 | 0.802 | 1.142 | Keep: A/B win |
| RPUSH 12 values | 618,824 | 558,775 | 0.903 | 1.276 | Keep: A/B win |
| RPUSH 16 values | 551,333 | 455,962 | 0.827 | 1.174 | Keep: A/B win |
| SADD 5 values   | 896,106 | 758,819 | 0.847 | 1.100 | Keep: A/B win |
| SADD 8 values   | 660,337 | 766,967 | 1.161 | 1.223 | fr faster |
| SADD 12 values  | 506,039 | 670,508 | 1.325 | 1.114 | fr faster |
| SADD 16 values  | 395,918 | 623,214 | 1.574 | 1.207 | fr faster |

Correctness: `scripts/keyed_write_packet_differ.py` PASS against Redis 7.2.4 on fresh ports,
covering LPUSH/RPUSH/SADD/ZADD N=4..19, HSET N=4..20, MSET fallback. Decision: **keep the
5-16 exact keyed-write parser backlog**. The ladder is real for RPUSH/SADD, but it does not
close LPUSH; LPUSH remains part of the existing structural `ChunkedList` gap rather than a
recent parser regression.

## Cod-b exact eight-key EXISTS parser (MEASURED 2026-06-19)

Criterion harness added in `fr-bench`: `cargo bench -p fr-bench --bench exists_vs_redis
-- --noplot`, oracle Redis 7.2.4 at
`/dp/frankenredis/legacy_redis_code/redis/src/redis-server`. Clean release binaries were rch-built
from detached worktrees at `03709a07c`: one clean `HEAD`, one clean `HEAD` with only the
`frankenredis-z3yrs` eight-key `EXISTS` parser removed. The workload initializes `k0..k7` and
times 128-command pipelines of 8-key `EXISTS` all-hit, half-hit, and duplicate-key mixes.

| Workload | Redis cmds/s | fr HEAD cmds/s | fr/redis | fr no-z3yrs cmds/s | HEAD/no-z3yrs | Verdict |
|---|--:|--:|--:|--:|--:|---|
| EXISTS 8 all hit | 1,124,940 | 866,759 | 0.770 | 776,600 | 1.116 | z3yrs keep; workload gap remains |
| EXISTS 8 half hit | 1,089,832 | 860,349 | 0.789 | 812,086 | 1.059 | z3yrs keep; workload gap remains |
| EXISTS 8 duplicates | 1,042,333 | 892,906 | 0.857 | 807,226 | 1.106 | z3yrs keep; workload gap remains |

Decision: keep `frankenredis-z3yrs`. The exact eight-key parser improves same-HEAD throughput by
5.9-11.6%, so it is not a revert candidate. Release-readiness impact is still negative for this
workload: clean FrankenRedis remains Redis-faster/Redis-wins on all three 8-key `EXISTS` mixes.
Focused parser tests passed; full `fr-conformance` was rerun for this closeout.

## Cod-b 8-key EXISTS encoded reply (MEASURED 2026-06-19)

Follow-up for `frankenredis-upx5x`: keep the borrowed `EXISTS` `_into` path that writes the integer
reply directly and returns `FastEncodedReply` from the server hot path. The parser-order and
no-expiry-store experiments were rejected/reverted; this is the only production lever kept from the
pass.

| Workload | Control fr/redis | Candidate fr/redis | fr candidate/control | Release-readiness impact |
|---|--:|--:|--:|---|
| EXISTS 8 all hit | 0.719 | 0.808 | 1.149 | improves, still Redis loss |
| EXISTS 8 half hit | 0.768 | 0.803 | 1.239 | improves, still Redis loss |
| EXISTS 8 duplicates | 0.785 | 0.895 | 1.317 | improves, still Redis loss |

Validation: `cargo test -p fr-runtime plain_exists_borrowed -- --nocapture`, targeted
`cargo check`/`clippy`, full `fr-conformance`, and `cargo fmt --check` all passed using the
compiler-scoped target under `/data/projects/.rch-targets/frankenredis-cod-b`. Redis-relative
score remains **0 wins / 3 losses / 0 neutral** for this focused suite, so `EXISTS` stays a
release-performance gap even after the keeper.

## Cod-b residual 8-key EXISTS runtime accounting (MEASURED 2026-06-19)

Follow-up for `frankenredis-qk0nm`: no production lever kept. Four runtime/accounting candidates
were measured and reverted: small pre-encoded integer replies, exact-8 runtime unrolling, batch
`exists_no_touch` hit/miss aggregation, and exact-8 specialization inside that batch helper.

| Candidate | all-hit fr/redis | half-hit fr/redis | duplicate fr/redis | Release-readiness impact |
|---|--:|--:|--:|---|
| Control after upx5x | 0.864 | 0.874 | 0.763 | Redis wins all cells |
| Small integer reply table | 0.754 | 0.812 | 0.839 | rejected |
| Runtime exact-8 unroll | 0.777 | 0.755 | 0.769 | rejected |
| Batch `exists_many_no_touch` | 0.812 | 0.812 | 0.835 | rejected |
| Exact-8 batch helper | 0.789 | 0.807 | 0.822 | rejected |

Validation during candidates: focused `fr-store` and `fr-runtime` tests passed; no qk0nm source
hunk remains. RCH release build succeeded, but remote bench failed on `FR_SERVER_BIN` path
rewriting; accepted timing artifacts used the local compiler-scoped subtarget under
`/data/projects/.rch-targets/frankenredis-cod-b`. Redis-relative score remains **0 wins / 3 losses /
0 neutral** for the focused `EXISTS` suite.

## Cod-a remaining quicklist2 RESTORE materialization gap (MEASURED 2026-06-19)

Follow-up for `frankenredis-k263a`: no production lever kept. The candidate fused listpack-span
decode with canonical growth-state byte totals and seeded restored `ListValue` metadata from those
totals. Focused correctness guards passed, but the Redis-vs-FrankenRedis Criterion gate showed no
statistically significant improvement and the median FrankenRedis throughput moved slightly down.

| Run | Redis elems/s | fr elems/s | fr/redis | Release-readiness impact |
|---|--:|--:|--:|---|
| Control after tnv37 | 135.51 K | 56.476 K | 0.417 | Redis faster |
| Fused stats candidate | 133.17 K | 55.544 K | 0.417 | rejected; no hunk remains |

Validation while the candidate was present: focused `fr-persist` and `fr-store` tests passed via
RCH, and the release server/bench build passed via RCH. The production hunk was reverted, so the
scorecard remains unchanged: QUICKLIST_2 `RESTORE ... REPLACE` is still a Redis-relative loss,
with **0 wins / 1 loss / 0 neutral** for this focused gate. Next work should target runtime/server
request materialization or direct quicklist object construction, not listpack growth-stat fusion.

## Cod-a RESP CRLF memchr scanner (MEASURED 2026-06-19)

Follow-up for `frankenredis-h6ppr`: no production lever kept. The candidate replaced
`fr-protocol::read_line`'s byte loop with `memchr::memchr`. It preserved parser behavior in focused
guards, and the initial Redis-relative GET/SET harness showed FrankenRedis still faster than Redis
7.2.4 in all four measured cells, but the current-vs-control keep gate failed after low-CV
confirmation.

| Workload | current/control | cv quality | Release-readiness impact |
|---|--:|---|---|
| GET P16 | 0.999 | clean | neutral |
| SET P16 | 1.018 | clean | small win |
| GET P128 | 0.959 | clean | rejected regression |
| SET P128 | 0.998 | clean | neutral |

Decision: revert h6ppr. The final lever score is **1 win / 1 loss / 2 neutral**, and the clean
P128 GET regression is enough to reject the scanner rewrite. Redis-relative GET/SET remains
favorable on this harness after reverting; the pass adds negative evidence only.
