# Performance Negative-Evidence Ledger (frankenredis vs redis 7.2.4)

Purpose: stop the perf agents (cc, cod-b, CrimsonFalcon, …) from re-treading levers
already proven to NOT win, and record where the real residual gaps live + who owns them.
Append measured results; never delete a row — a "tried, didn't win" entry is the point.

Convention: ratios are fr/redis (>1.0 = fr slower / more RAM). "Measured" = ran a real
release A/B; "Reasoned" = algorithmic certainty without a release bench (cargo-check-only
turns). Keep claims honest — mark which.

## Established baseline (do NOT re-litigate)
- **Throughput is parity-or-faster** on hot commands (GET ~1.2x faster, SET ~1.3x faster,
  HSET 1.13x the lone residual). The historical "fr 2x slower pipelined" (ohsk5) headline
  is CLOSED. Tools: scripts/profile_hot_path.sh, scripts/perf_gap_dashboard.sh.
- **Cold-command dispatch**: ~20 cold commands were the real pipelined-CPU gap (per-cmd
  dispatch machinery, not alloc) — all converted to borrowed fast paths; now 0.81–1.95x.
  Pattern: add a PlainXCmd enum + execute_plain_X_borrowed. (See git log perf(fr-runtime).)
- **Reply path is mature**: borrowed zero-copy args, itoa2 two-digit table (push_i64/usize),
  direct-encode header helpers (push_array_header/push_map_header), 30+ borrowed_plain_*
  fast paths in fr-server. decimal_*_len now branchless ilog10 (e4fu8).

## Rejected levers — measured REGRESSION or no-win (do NOT retry)
| Lever | Result | Why |
|---|---|---|
| `frankenredis-ohsk5` cod-b INCR expiry/store-probe consolidation | `INCR 0.9886x` candidate/control; guards `SET/GET/HSET 0.9377/0.9558/0.8146x`; rejected candidate vs Redis `INCR/GET/LPUSH/RPUSH/ZADD 0.78/0.66/0.75/0.78/0.74x`; current-control vs Redis `INCR 0.94x`, `SET/GET/HSET 1.04/1.00/1.06x`, `LPUSH/RPUSH/SADD/ZADD 0.71/0.81/0.87/0.79x` | Collapsing `drop_if_expired` + `key_has_expiry` before `entries.get_mut` preserved focused `INCR` tests and `fr-store` check, but did not improve INCR and regressed guard workloads. Source hunk reverted before commit. Artifact: `artifacts/optimization/frankenredis-ohsk5-incr-store-probe-codb/20260620T105145Z/`. |
| `frankenredis-ohsk5` cod-b batch-local RESP3 reply-mode cache for server fast paths | GET `1.02x` candidate/control; guards SET/INCR/HSET/MSET `1.01/0.95/0.98/1.02x` | Noise-scale target movement did not close the fresh current-vs-Redis GET loss (`0.83x`), and `INCR` softened. Source hunk reverted/not shipped. Artifact: `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T061925Z-resp3-cache-candidate/candidate_vs_control_get_guard_20260620T0626Z.txt`. |
| `frankenredis-ohsk5` cod-b skip plain-GET fast active-expire call when `count_expiring_keys()==0` | GET `1.01x` candidate/control; guards SET/INCR/HSET/MSET `0.99/0.97/0.95/1.01x` | The existing active-expire function already has a no-expiring-keys fast no-op; skipping the call outside it was neutral-to-soft-loss. Candidate stayed isolated in clean worktree, not shipped. Artifact: `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T0630Z-get-expire-count-gate/candidate_vs_control_get_guard_20260620T0632Z.txt`. |
| Hand-rolled large-buffer reuse / malloc-avoidance | 0.77–0.93x (REGRESSION) | mimalloc (fr default) already recycles large buffers; hand reuse fights it. A/B before trusting any malloc-avoidance lever. |
| ChunkedList → VecDeque / decode-path rewrite for list RESTORE | 0.53x (SLOWER) | per-element alloc is the cost; VecDeque didn't help. Real lever = packed-listpack-node ChunkedList (99fwc), not container swap. |
| SWAR/SIMD on memory-bound byte loops (max/copy/fill, HLL register-max) | ~1.0x (0.94x for HLL) | only COMPUTE-bound loops win (popcount/CRC/bitwise = 4–13x). Check compute-vs-memory first. Clean-crate compute kernels already done. |
| used_memory via counting-allocator | ~7% throughput hit + wrong target | estimate_memory_usage_bytes MODELS redis; counting-alloc measures fr's actual RAM (a different number). RSS lags frees. Don't "fix" the model with real accounting. |
| zadd 8% pipelined gap | WONTFIX (x1zbp) | distributed across dispatch, no single hot spot. |
| zset DUMP integer-score listpack shortcut in `Store::dump_key` | mixed then rejected: 1.0805x candidate/control in first low-CV A/B, then 0.9559x candidate/control in stronger 500k/9-trial confirmation | Correctness guard passed, but throughput did not hold. The real DUMP gap is structural compact-zset listpack rebuild/serialization, not just skipping score string reparse. Do not extend this micro-lever without an isolated retained-listpack/cached-DUMP representation and same-current A/B proof. |

## Real residual gaps (structural; mind ownership before touching)
| Gap | Ratio | Owner / bead | Note |
|---|---|---|---|
| Fresh cod-b P16/c50 Redis-benchmark residuals | GET 0.83x, LPUSH 0.84x, RPUSH 0.74x, SADD 0.73x, ZADD 0.69x | cod-b / ohsk5, store lanes held by BlackThrush during this pass | Artifact: `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T061610Z-redis-benchmark-current/current_vs_redis_redis_benchmark.txt`. Non-store GET probes above did not pay; the largest confirmed gaps are list/set/zset store work. |
| Fresh cod-b control rerun after rejected INCR probe | INCR 0.94x, SET 1.04x, GET 1.00x, HSET 1.06x, LPUSH 0.71x, RPUSH 0.81x, SADD 0.87x, ZADD 0.79x | cod-b / ohsk5 | Artifact: `artifacts/optimization/frankenredis-ohsk5-incr-store-probe-codb/20260620T105145Z/control_vs_redis.txt`. Confirms current-control is near parity on scalar commands and still loses on list/set/zset writes. |
| Keyspace dict RAM | ~4.5–5.4x | cod-b / uhthd, also project_keyspace_ram_gap | ordered_keys is the COST of deterministic sorted SCAN (encoded in core_scan.json + tests). Cutting it to ~2x = a SCAN-semantics design reversal (hash-order + reverse-binary cursor), multi-day, all-or-nothing. Not a clean opt. |
| zset RAM | ~1.54x | CoralOx / uybhq | peni2 (Arc<[u8]> shared members) shipped; residual is structural (dict IndexMap + ordered BTreeMap each hold score+node overhead). Measure fresh-process RSS, not used_memory. |
| list DUMP | ~1.7–2.7x | open / 99fwc | ChunkedList re-synthesizes Owned chunks per DUMP. Lever = make push build Listpack chunks (infra exists). BIG. |
| Per-cmd CPU (deep pipeline, remaining) | varies | cod-b / ohsk5 | the cold-cmd vein is done; remaining is in their domain. |
| "Close ALL gaps, pure safe Rust" | — | CrimsonFalcon / gu5nf | broad; coordinate before hot-path edits to fr-store/fr-runtime/fr-server. |

## Methodology gotchas (cost real hours)
- **A stray PSYNC replica on single-threaded fr craters ALL bench ratios** — always check
  `INFO replication | connected_slaves:0` before trusting a regression.
- **rch builds the LOCAL tree** (perpetually behind origin when committing from worktrees)
  and does NOT sync the linked binary — copy from /data/tmp/cargo-target, sanity-check vs a
  known-fixed behavior before trusting any "divergence".
- **Config pollution on the shared oracle** (e.g. list-max-listpack-size set by a peer) →
  false parity-gate fails. Reset config / launch a fresh oracle.
- **Pipeline the DUMP/throughput bench** — single-conn is syscall-masked and hides the gap.
- release builds only for perf; debug ratios are meaningless.

## Open clean levers (unclaimed, reasoned-promising)
- decimal_*_len branchless ilog10 — DONE (e4fu8, reasoned; criterion A/B pending batch).
- listpack integer decode itoa2 — DONE (vqjz1, f5e835d45→2648d9e6f; reasoned). fr-persist
  ListpackIntegerBytes::new now reuses fr-protocol write_u64_digits (now `pub`) instead of
  a single-digit div-by-10 loop. Path: RESTORE / DEBUG RELOAD / RDB-load of int-bearing
  listpack collections. Byte-identical (boundary + i64-extreme test). Criterion RESTORE
  A/B pending batch. NOTE: write_u64_digits is now the shared canonical itoa — other
  int-renderers (redis_score_to_string, RESP2 zset scores) could reuse it, but their
  callers live in contended fr-runtime/fr-store; coordinate before wiring.
- frankenredis-cod-a-packed-int-itoa-tgr69 / cod-a: fr-persist packed/RDB integer
  decode materialization now reuses the shared itoa2 helper for listpack entry
  `to_bytes` / `into_bytes`, stream listpack integer fields, legacy ziplist
  integer entries, intset members, and RDB integer-encoded strings — CODED
  (reasoned; batch benchmark pending). Guard covers i64 sign/edge bytes and
  packed integer decode output. Retry condition if rejected: only revisit with a
  fresh RESTORE / DEBUG RELOAD / RDB-load profile where decimal integer
  materialization is named, not as generic formatting cleanup.
- 17-value LPUSH/RPUSH/SADD exact borrowed packet parser — CODED in `fr-server`
  under `frankenredis-ohsk5` (reasoned; batch benchmark pending). Retry condition:
  keep only if the next release A/B for keyed-values packets shows a stable win;
  otherwise move this row to rejected and stop extending exact arities.
- 18-value LPUSH/RPUSH/SADD exact borrowed packet parser — CODED in `fr-server`
  under `frankenredis-ohsk5` (reasoned; batch benchmark pending). It extends the
  current contiguous exact-arity ladder by one realistic pipelined batch without
  changing malformed/noncanonical fallback behavior. Retry condition: do not add
  19+ keyed-values exact arities unless release A/B names this exact-parser family
  as a stable keyed-values win.
- two-field HSET exact borrowed packet parser — CODED in `fr-server` under
  `frankenredis-ohsk5` (reasoned; batch benchmark pending). Reuses the existing
  multi-pair borrowed HSET runtime path for canonical `HSET key f1 v1 f2 v2`;
  single-field HSET stays on its existing fast path and larger/odd arities stay
  generic. Retry condition if rejected: only revisit HSET arity specialization
  with a release A/B that isolates HSET field-pair pipelines.
- MSET exact-parser prefix dispatcher — CODED in `fr-server` under
  `frankenredis-ohsk5` (reasoned; batch benchmark pending). The server now
  selects the 2..8-pair exact MSET parser by canonical RESP array header instead
  of probing lower arities first; noncanonical, single-pair, 9+ pair, limited,
  and malformed inputs still fall through to generic parsing. Retry condition if
  rejected: do not add more MSET exact arities unless a profile names the MSET
  parser probe chain or a release A/B isolates MSET arity-mix pipelines.
- batch-cached borrowed write gate — CODED in `fr-server` / `fr-runtime` under
  `frankenredis-ohsk5` (reasoned; batch benchmark pending). The buffered
  multibulk loop now computes the expensive default write fast-path predicate
  once per batch for canonical SET/MSET/HSET exact packets, matching the existing
  cached GET read gate and invalidating on generic state-changing dispatch. Guard
  proves a cached true gate before `SELECT 1` cannot leak the following `SET`
  through the db0 fast path. Retry condition if rejected: do not add more cached
  gate variants unless a profile names default write-gate/ACL/session predicates
  on SET/MSET/HSET pipelines; route instead to output/batch arena or key-layout
  work.
- frankenredis-15lug / cod-b: uppercase no-arg multibulk `PING` literal parser
  fast path — CODED in `fr-server` (reasoned; batch benchmark pending). Target:
  pass195 residual `ping_mbulk` (~0.94x) where inline PING is already fr-faster.
  The hot Redis-benchmark shape `*1\r\n$4\r\nPING\r\n` now bypasses the
  case-insensitive borrowed parser while parser limits, mixed-case PING, message
  PING, noncanonical packets, subscriber mode, and transactional cases keep the
  existing fallback behavior. Retry condition if rejected: do not add more PING
  parser literals unless `perf_baseline_capture.py --trials` confirms
  `ping_mbulk` as a low-CV residual and a profile names this parser branch.
- frankenredis-h6ppr / cod-a: `fr-protocol` CRLF line scan via locked
  `memchr::memchr` — MEASURED REJECTED 2026-06-19 and source hunk reverted.
  Longer current-vs-control confirmation produced 1 win / 1 loss / 2 neutral
  across GET/SET P16/P128, including a clean 0.959x current/control loss on
  GET P128. Retry condition: only revisit CRLF line scanning with fresh parser
  self-time evidence and a low-CV benchmark that isolates line scanning from
  runtime/server packet-parser work.
- frankenredis-bssrh / cod-a: `fr-store` listpack sizing canonical-integer
  probe now avoids `value.to_string().as_bytes() == entry` and uses an
  allocation-free canonical byte predicate before parsing — CODED (reasoned;
  batch benchmark pending). Path: list/packed-list sizing and encoding decisions
  for integer-looking members during SADD/LPUSH/RESTORE/DUMP workloads. Guard
  compares the new predicate and listpack byte sizing against the old round-trip
  behavior across zero, `-0`, leading-zero, plus-sign, i64 min/max, overflow, and
  encoding-width boundaries. Retry condition if rejected: only revisit with a
  profile naming `list_lp_int`/listpack sizing, not as generic integer cleanup.
- frankenredis-087qq / cod-a: `fr-store` integer value materialization now routes
  `Value::Integer` owned-byte paths and intset member bytes through the shared
  `integer_decimal_bytes` / itoa2 writer instead of `i64::to_string()` formatting
  machinery — MEASURED KEEP 2026-06-19. Focused fresh-server release A/B used
  the new `fr-bench --workload integer-get` harness, prefilled keys via `INCRBY`
  so both engines store integer-encoded string values, then timed `GET` against
  Redis 7.2.4 at p1/p16/p128. Result: integer-get fr/redis throughput
  1.008 / 1.095 / 1.353; win/loss/neutral 3/0/0 for the target cells and 9/0/0
  including ordinary GET/SET control cells. Several cells are noisy (`cv_pct > 5`)
  under shared-host contention, so the direction is keep-quality but publication
  numbers still need quiet-host rerun (`vibu6`). Raw artifact:
  `artifacts/optimization/frankenredis-087qq/verify_integer_get_20260619T0505Z/summary.json`.
  Validation: focused `fr-bench` fmt/clippy/tests passed, release binaries were
  rch-built, and the full workspace gates are green after resolving closeout-only
  gate debt: `cargo check --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`,
  and refreshed `cargo test -p fr-conformance -- --nocapture` all passed (`rch`
  for the build/check/clippy/conformance gates).
  Scope is store-side byte
  materialization for integer GET-like paths and `SetValue::Int` iteration /
  promotion / removal; RESP serializer, runtime, and server code are unchanged.
  Guard pins zero, sign edges, and i64 min/max against the old `to_string`
  reference for `Value::Integer` and intset member materialization. Retry
  condition: only revisit integer materialization after a fresh profile names
  this path again or quiet-host verification contradicts the measured keep.
- frankenredis-gu5nf.32 / cod-a: `fr-store` `SetValue::retain` stack-borrowed
  decimal bytes for intset predicates — MEASURED REJECTED 2026-06-19 and source
  hunk reverted. Criterion set-algebra vs Redis 7.2.4 showed no Redis-relative
  keep signal on the target `SINTERSTORE` / `SDIFFSTORE` mixed-encoding path,
  and `SDIFFSTORE` improved after reverting the candidate. Do not retry intset
  predicate byte formatting unless a fresh profile names `SetValue::retain` or
  mixed intset/generic set-algebra allocation cost as a top hotspot, and a
  before/after Criterion run beats the reverted baseline.
- frankenredis-n2u1g / cod-b: zset score direct encoder for borrowed `ZSCORE`
  and `ZMSCORE` network fast paths — MEASURED KEEP 2026-06-19.
  `fr-protocol::encode_redis_double` writes Redis d2string bytes directly into
  RESP3 Double / RESP2 bulk-string frames, and fr-runtime/fr-server now use it
  for score-read fast paths instead of allocating a `String`/score `RespFrame`.
  Guard compares raw wire bytes against generic dispatch for RESP2, RESP3, nil,
  and WRONGTYPE paths. The focused `zrange-withscores` head-to-head below also
  covers the option-bearing direct encoder, so this is no longer pending.
- frankenredis-n2u1g / cod-b: direct encoder for canonical rank-form
  `ZRANGE key start stop WITHSCORES` — MEASURED KEEP 2026-06-19. Focused
  fresh-server `release-perf` A/B used the dedicated
  `fr-bench --workload zrange-withscores` harness at p1/p16/p128, 200k requests,
  4 clients, 5 trials, against vendored Redis 7.2.4. Result: fr/Redis
  throughput `1.084 / 1.283 / 1.378`; win/loss/neutral `3/0/0`. p1 is noisy
  because Redis cv was 5.94%, but p16 and p128 are clean (`cv_pct < 5`) and
  have lower fr p99s (`307us` vs `486us`, `2401us` vs `3937us`). Raw artifact:
  `artifacts/optimization/frankenredis-n2u1g/verify_zrange_withscores_20260619T0515Z/summary.json`.
  Conformance guard: `zset_score_emit_differ.py` passed byte-exact vs Redis
  7.2.4 across ZSCORE/ZMSCORE/ZINCRBY/ZADD-INCR/WITHSCORES/ZPOPMIN/ZPOPMAX and
  RESP2+RESP3 Double output. Generic `REV`/`BYSCORE`/`BYLEX`/`LIMIT` option
  shapes still fall through to canonical dispatch. Retry condition: do not
  expand to `ZREVRANGE`, `ZRANGEBYSCORE WITHSCORES`, or `ZRANGE ... LIMIT`
  direct encoders unless a fresh focused bench or release profile isolates those
  exact option shapes as score-format/allocation bottlenecks.
- frankenredis-mixed-zset-listpack-direct-emit-vly2n / cod-a: `fr-persist`
  compact zset listpack encode now streams member/score entries directly for
  mixed integer/fractional score sets instead of building `score_bytes` and
  `flat` temporary vectors — CODED (reasoned; batch benchmark pending).
  Integer-valued scores use the stack `decimal_i64_scratch` path; fractional
  score formatting remains unchanged. Guard pins mixed-score ordering,
  same-score member tie ordering, and decoded listpack entry bytes. Retry
  condition if rejected: only revisit with a fresh mixed-score compact-zset
  DUMP/RDB profile naming listpack construction or score formatting, not as
  generic vector cleanup.
- frankenredis-hash-listpack-direct-emit-dv9n5 / cod-a: `fr-persist`
  compact hash listpack encode now streams field/value entries directly into
  the listpack payload instead of allocating a `Vec<&[u8]>` staging array before
  calling `encode_listpack_strings_blob` — CODED (reasoned; batch benchmark
  pending). The shared listpack finalizer keeps header/terminator/count behavior
  identical for normal listpacks and the existing zset direct encoder. Guard
  compares direct hash listpack bytes against the old flat-entry reference and
  decodes integer/string/null-byte field-value pairs. Retry condition if
  rejected: only revisit with a fresh compact-hash DUMP/RDB profile naming
  listpack construction, not as generic vector-elision cleanup.
- frankenredis-set-intset-canonical-noalloc-acetq / cod-a: `fr-persist`
  compact set intset selection now reuses the shared allocation-free canonical
  decimal parser instead of validating each parsed member by allocating
  `value.to_string()` and comparing bytes — CODED (reasoned; batch benchmark
  pending). Guard compares intset selection against the old parse+to_string
  round-trip oracle across canonical, noncanonical, overflow, whitespace, and
  invalid-UTF8 members. Retry condition if rejected: only revisit with a fresh
  integer-heavy compact-set DUMP/RDB profile naming intset canonicalization, not
  as generic decimal-format cleanup.
- frankenredis-set-listpack-direct-emit-tpans / cod-a: `fr-persist`
  compact set listpack encode now streams set members directly into the shared
  listpack finalizer instead of allocating a `Vec<&[u8]>` staging array before
  `encode_listpack_strings_blob` — CODED (reasoned; batch benchmark pending).
  Guard compares direct set listpack bytes against the old flat-entry reference
  and decodes string, positive-integer, negative-integer, and null-byte members.
  Retry condition if rejected: only revisit with a fresh compact-set DUMP/RDB
  profile naming listpack construction, not as generic vector-elision cleanup.
- frankenredis-g5o8d / cod-a: `fr-persist` compact list QUICKLIST_2 PACKED
  nodes now encode listpack entries directly into the node payload while
  scanning instead of retaining a per-node `Vec<&[u8]>` and re-encoding on
  flush — CODED (reasoned; batch benchmark pending). Guard compares mixed
  PACKED/PLAIN/PACKED quicklist2 bytes against the old flat-entry reference and
  decodes node containers/listpack entries. Retry condition if rejected: only
  revisit with a fresh mixed list DUMP/RDB profile naming packed-node listpack
  construction, not as generic Vec cleanup.
- frankenredis-k1wcp / cod-a: `fr-store::encode_dump_quicklist2` fallback now
  counts quicklist2 nodes in a cheap pass and emits PLAIN/PACKED records
  directly while scanning instead of building a `Vec<Node>` plus per-node
  `Vec<&[u8]>` staging — CODED (reasoned; batch benchmark pending). Guard pins
  mixed PACKED/PLAIN/PACKED DUMP output with old-reference listpack bytes for
  both packed nodes and decoded PLAIN content. Retry condition if rejected: do
  not retry quicklist fallback node-vector removal unless a fresh list DUMP
  profile names fallback node construction.
- frankenredis-lbmk6 / cod-a: `fr-store::dump_key` set-listpack branches now
  stream `SetValue` members directly into the store-local listpack finalizer
  instead of cloning into `Vec<Vec<u8>>` and staging `Vec<&[u8]>` — CODED
  (reasoned; batch benchmark pending). Generic sets borrow member bytes; intset
  members use stack decimal bytes before `encode_listpack_entry`. Guard compares
  generic and intset-backed set listpacks against the old flat-reference encoder
  and decodes binary/integer-looking/signed members. Retry condition if
  rejected: do not retry set-listpack vector cleanup unless a fresh SET
  DUMP/RDB profile names listpack construction.
- frankenredis-knzdi / cod-a: `fr-persist` RDB listpack decode for compact
  set/hash/zset now consumes owned decoded entries with `into_bytes()` instead
  of cloning string payloads through `to_bytes()` and dropping the original —
  CODED (reasoned; batch benchmark pending). Integer listpack entries still
  render through the same canonical decimal helper, and output ordering/types
  stay unchanged. Guard is the compact set/hash/zset listpack decode suite plus
  crate-scoped check. Retry condition if rejected: do not retry owned-entry
  move cleanup unless a fresh DEBUG RELOAD / RESTORE profile names compact
  collection listpack decode allocation.
- frankenredis-ta8s1 / cod-a: `fr-persist` RDB QUICKLIST_2 PACKED list decode
  owned-entry move (`into_bytes()` instead of `to_bytes()`) — MEASURED
  REJECTED 2026-06-19 and source hunk reverted. Focused Criterion RESTORE
  head-to-head against Redis 7.2.4 measured only 0.371x Redis throughput
  (fr median 87.777 K restores/s, Redis median 236.90 K restores/s; fr time
  2.699x Redis). Do not retry quicklist2 owned-entry move cleanup unless a
  fresh profile names packed quicklist2 listpack decode allocation and a
  same-worker Criterion A/B beats the reverted baseline.
- frankenredis-ohsk5 / cod-b: `fr-store` compact hash duplicate-field
  overwrite now uses a borrowed `CompactFieldMap::insert_borrowed` path for
  hashtable-range hashes instead of allocating the old value only to discard it;
  same-length value overwrites rewrite the arena slot in place instead of
  appending dead record bytes — CODED (reasoned from prior HSET duplicate-field
  residuals; batch benchmark pending). Guard extends the CompactFieldMap
  IndexMap-equivalence stream with borrowed upserts and pins arena/no-dead-byte
  behavior for same-sized overwrites. Retry condition if rejected: do not add
  further HSET allocation micro-levers unless a focused HSET profile still names
  compact-hash duplicate overwrite / arena churn after this path.
- frankenredis-uhthd / cod-b: `fr-store` KeyDict primitive now stores chaining
  nodes in a safe arena (`Vec<Option<Node>>` + free-list indices) instead of one
  `Box<Node>` allocation per key — CODED (reasoned from pass226 rejection, where
  half-wired KeyDict was too slow while still preserving side indices; batch
  benchmark pending). Guard keeps the existing insert/get/remove/SCAN/random
  sampling equivalence tests and adds a churn test proving removed node slots are
  reused without growing the arena. Retry condition if rejected: do not repeat
  main-table-only KeyDict wiring; retry only with full side-index-removing
  Store integration (native SCAN/RANDOMKEY/eviction) or a focused KeyDict bench
  showing the arena primitive itself as the remaining bottleneck.
- frankenredis-uhthd / cod-b: `fr-store` KeyDict primitive now supports
  presized bulk builds (`with_capacity`/`reserve`) and grows before linking a
  new node at load-factor overflow, avoiding repeated bucket rebuilds and the
  insert-then-immediate-rehash path during future Store.entries migration —
  CODED (reasoned from pass226's 1M-key load stall and the graveyard
  resize/allocation-churn guidance; batch benchmark pending). Guard proves a
  4096-key presized build does not resize, preserves get/SCAN/random_sample
  semantics, and adds an ignored bulk-build timing hook for batch proof. Retry
  condition if rejected: do not claim this as an end-user RAM win by itself;
  only retry if focused KeyDict build benchmarks still name resize/allocation
  churn, or with full side-index-removing Store integration.
- frankenredis-uhthd / cod-b: `fr-store` RANDOMKEY's per-db `Arc<[u8]>`
  sampling vectors are now dirty lazy caches instead of resident side indices
  maintained on every key insert/delete, and the now-unused `Entry.random_slot`
  back-index is removed behind a stricter `Entry <= 48B` compile-time guard —
  CODED (reasoned from the 1M-key RSS gap and prior `random_key_positions`
  win; batch RSS/throughput and Redis-oracle RANDOMKEY/SCAN proof pending).
  Guard adds a lazy-rebuild reachability test proving write-only loads keep the
  RANDOMKEY vector empty until first use, then rebuild from live entries after
  churn; local gate was `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo check -p fr-store --all-targets`.
  Retry condition if rejected: do not reintroduce an always-resident RANDOMKEY
  side vector; only revisit with a benchmark showing RANDOMKEY-heavy workloads
  dominate and requiring an incremental cache-maintenance mode behind the same
  Store-level invariants.
- frankenredis-uhthd / cod-b: `fr-store` sorted `ordered_keys` is now a dirty
  lazy side index for ordinary write-heavy keyspaces, with SCAN/KEYS/SWAPDB
  boundaries rebuilding it from canonical `entries` only when sorted traversal is
  requested, and `all_keys()` preserving deterministic snapshot/debug order from
  `entries` without forcing residency — CODED (reasoned from the persistent
  keyspace-RAM gap after lazy RANDOMKEY; batch RSS/throughput and SCAN-heavy
  regression proof pending). Guard proves SET-only loads keep `ordered_keys`
  empty, ordered reads rebuild it exactly once, `all_keys()` remains sorted while
  non-resident, and the next structural write drops the index again; local gate
  was `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo check -p fr-store --all-targets`.
  Retry condition if rejected: do not restore always-resident sorted key storage
  for generic workloads; only add incremental maintenance if a SCAN/KEYS-heavy
  profile shows rebuild churn dominating after this memory win.
- frankenredis-uhthd / cod-b: MEASURED gauntlet for lazy sorted `ordered_keys`
  on current `4cf73ebef` vs vendored Redis 7.2.4. Release build:
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo build --release -p fr-server -p fr-bench`.
  Fixed the baseline scripts first after an invalid 299xx run exposed a same-port
  allocator collision under peer benchmark load; do not use all-equal-RSS
  `.bench-history` runs from the old allocator as evidence. Valid high-port run
  (`FR_BENCH_PORT_BASE=42051`, 200k scale) measured keyspace RSS at **1.912x
  Redis** (`fr_rss=30,515,200`, `redis_rss=15,958,016`) and memory geomean
  **1.315x**; this is still not domination, but it improves on the prior
  documented post-pass225 residual **2.59x**. Throughput quick matrix
  (`FR_BENCH_PORT_BASE=42151`) had only three low-noise cells: `set@p1=1.054x`
  win, `hset@p1=0.901x` loss, `incr@p1=0.993x` neutral/loss; 33 cells excluded
  as noisy. Targeted SCAN guard passed; 100k-key load was `0.963x`, first full
  `SCAN COUNT 1000` was `0.985x`, warm full SCAN was `1.039x`. Decision: KEEP,
  no revert. Retry condition: next `uhthd` work must attack the remaining
  1.91x keyspace RSS gap or collection RSS gaps; do not claim parity from
  `used_memory` alone, and always run high-port distinct Redis/fr pairs.
- frankenredis-uhthd / cod-a: INVALIDATED default-port gauntlet attempt against
  `4cf73ebef` and parent `10af233f4`. The run used the old/default 299xx port
  pair under peer benchmark load and produced suspicious all-equal/parity RSS
  cells (`full keyspace 1.003x -> 1.000x`) plus a low-CV `mixed@p16=0.956x`
  ratchet failure. Do not use this as keep/reject proof: the high-port rerun
  above supersedes it, and the temporary local rollback was backed out. Decision:
  no revert from this invalidated run. Retry condition: every future Redis/fr
  head-to-head must use distinct high-port pairs (`FR_BENCH_PORT_BASE`) or an
  explicitly isolated quiet host before making a code decision.
- (add here as found) — prefer clean crates (fr-protocol, fr-persist non-LZF) not under a
  peer's active reservation; bench A/B in release before claiming a win.

## cc session 2026-06-18 (cod-walled; cc-carries) — PRESIZE LEVERS shipped + same-class status
- **SHIPPED 71a908f75 (Reasoned):** presize the collection listpack-blob builders
  (encode_set/hash/zset listpack blob) to one allocation (safe upper bound: Σ(len)+n·~11+hdr)
  instead of growing from empty. Byte-identical (all 4 collection DUMP gates PASS, incl their
  DEBUG-RELOAD RDB-save-encoder step). Win on bulk RDB-save + listpack-collection DUMP. Retry: n/a (done).
- **SHIPPED c83e5e926 (Reasoned):** presize the quicklist node listpack buffer in BOTH encoders
  (fr-persist encode_compact_list_quicklist2 RDB-save + fr-store encode_dump_quicklist2 DUMP) to
  the per-node byte budget (cap 8 KiB = SIZE_SAFETY_LIMIT). Helps the common 1-2 node quicklist.
  Byte-identical (list_quicklist_dump_differ PASS + multinode/large-elem DUMP exact pre+post RELOAD).
- **intset encode now fully alloc-lean:** blob was already `with_capacity(8+len*width)` and the
  caller pre-sizes values/out, BUT a later re-examination found encode_intset_blob did a separate
  `values.to_vec()` before sorting — FIXED 78fff02e8 (sort owned values in place; sole caller
  discards them; byte-identical, intset gate PASS). Lesson: "pre-sized" != "alloc-free" — check for
  to_vec/clone copies too. Now genuinely optimal; do not re-examine.
- **ALREADY-OPTIMAL (do NOT re-examine):** the RESP reply path is NOT a fresh-Vec-per-
  reply churn target — fr-protocol `encode_into(out)` writes into the REUSED per-connection output
  buffer, and `to_bytes` uses `encoded_len_hint` to pre-size. Reply encoding is already allocation-lean.
- **DEAD-END (do NOT retry):** pre-sizing the OUTER multi-node accumulator `buf` in the quicklist
  encoders. Each node is rdb_encode_string'd with LZF compression, so the per-node serialized size is
  UNPREDICTABLE (compressible data → 10x smaller); reserving node_count·budget would massively
  over-allocate transiently on compressible lists. The per-node buffers (pre-compression, known size)
  were the only safe presize targets — done. Retry only with a measured node-size distribution.
- **SHIPPED ca61b6ca4 (Reasoned):** the DUMP-COMMAND side (fr-store) was NOT presized when the
  prior row claimed the class "exhausted" — encode_listpack_strings (backs hash + zset listpack
  DUMP) and encode_set_listpack_dump (set DUMP) built encoded_entries from empty. Now presized
  (precise from the entries slice / rough safe bound). Byte-identical (hash/zset/set/intset DUMP
  gates PASS). LESSON: there are TWO encoder sides per collection — fr-persist RDB-save
  (encode_compact_*) AND fr-store DUMP-command (encode_*_dump / encode_listpack_strings); audit
  BOTH, plus their intermediate buffers, before declaring a class done.
- The buffer-presize realloc-elimination class is now done across BOTH encoder sides (RDB-save +
  DUMP-command) for collection listpacks, quicklist nodes, and intset. Remaining same-class
  candidates would need a measured hot-spot (release profile) to justify, not blind extension.

## cc session 2026-06-18 (cod-walled; cc-carries) — REALISTIC-HOT RESP paths ALREADY alloc-optimal
- The GET/SET/command HOT path (request parse + reply) is NOT a clean-win target — audited and
  found already allocation-optimal (do NOT re-examine):
  - **Request argv parse**: parse_command_frame (owned) pre-sizes `Vec::with_capacity(count.min(1024))`;
    parse_command_args_borrowed_into_inner (the borrowed hot path, ohsk5) reserves
    `count.min(1024)` into the caller-reused argv buffer (1278-1281). Both cap at 1024 to bound a
    malicious huge `*N`. Borrowed path reuses the per-conn buffer (no per-command alloc).
  - **Reply encode**: fr-protocol `encode_into(out)` writes into the REUSED per-connection output
    buffer (no fresh Vec/reply); `to_bytes` pre-sizes via `encoded_len_hint`.
  CONSEQUENCE: the only clean-win vein cc found this session was the RDB-save/DUMP/MIGRATE encode
  path (7 levers, see manifest); the realistic GET/SET hot path is already optimized AND
  un-benchable under cargo-check-only. Headline-workload gains now need a release/rch profile to
  find a real hot spot, or are structural (CoralOx fr-store RAM). Do not blind-optimize the hot path.

## cc session 2026-06-18 (cod-walled; cc-carries) — DEAD-ENDS + CONVERGENCE (Reasoned)
- **DEBUG-build A/B is INVALID under cargo-check-only.** cc can build only debug binaries
  (no `--release`, no rch per directive); a debug-fr-server vs release-redis bench is
  apples-to-oranges (debug fr ~10-50x slower regardless of real perf). So cc CANNOT validly
  quantify a perf lever this session. Conformance/byte-output probes ARE valid on a debug
  build (output is build-profile-independent) — that is cc's productive lane. Retry condition:
  only run a throughput A/B from a RELEASE build (rch/criterion); never trust a debug A/B.
- **Conformance pillar has CONVERGED for cc.** Every surface probed this session came back
  BOTH byte-exact AND already-gated (206 differ/gate scripts exist): RESP3 aggregate-type
  emission (%map/~set/,double/=verbatim/*array — 5 resp3 gates), set encoding transitions
  (intset/listpack/hashtable at 512/513, 128/129, val>64), string int/embstr/raw (44B boundary,
  i64-overflow, force-raw — gated by n1i7i), string DUMP LZF (all sizes), cross-engine RESTORE
  BOTH directions (redis DUMP->fr and fr DUMP->redis correct for all types incl tombstoned
  stream + large-element quicklist). Retry condition: re-probe a surface only after a NEW lever
  touches it; do not add a 207th redundant gate (CHECK-BEFORE-CREATE — the harness is dense).
- **1z4ba (REAL bug, FIXED 83b9744b0):** quicklist element 8KiB..1GiB was emitted as a PLAIN
  node (RDB container 0x01) vs redis PACKED (0x02). redis isLargeElement = sz>=packed_threshold
  (1<<30, quicklist.c), NOT the per-node budget. Fixed both encoders (fr-store
  quicklist_plain_node_required + fr-persist encode_compact_list_quicklist2); verified DUMP
  byte-exact + RDB-save consistent. Both encoders now gated (dfce7321e + the collection family
  71b258d53 closed the parallel RDB-save-encoder gate gap for hash/set/zset/intset). Retry: n/a (done).
- **aapu4 = BY-DESIGN, NOT a bug (closed wontfix):** XDEL'd stream entries — redis retains
  listpack tombstones, fr's arena PackedStreamLog eagerly compacts them, so the raw DUMP blob
  differs in size. NON-CONTRACTUAL + NON-OBSERVABLE (XLEN/XINFO/XRANGE/DEBUG-DIGEST-VALUE all
  match, RESTORE round-trips both directions); stream_dump_reload_fuzz already documents this and
  asserts the contract not the bytes. Retry condition: do NOT re-file as a bug; only revisit if
  a future requirement demands byte-equal stream DUMP with redis (would need retain-until-rewrite
  semantics in PackedStreamLog = CoralOx fr-store structural, not a parity bug).

## MEASURED head-to-head vs Redis 7.2.4 (2026-06-19, cc, release build via rch) — VERIFY PHASE
Constraint lifted: rch release builds+benches allowed. First MEASURED numbers (were
commit-message-only). Harness: fr-bench --pipeline 16 --requests 300000 --trials 5 (8 for lpush),
fr-release @origin 4cf73ebef vs vendored redis-server 7.2.4, loopback. Full table +
caveats in docs/RELEASE_READINESS_SCORECARD.md. Sandbox-contended; cv>5% flagged.

- **HEADLINE CONFIRMED (Measured): throughput domination is REAL.** Geomean fr/redis = 1.348x
  over 9 workloads (depth16); fr faster on 8/9. The long-claimed "GET ~1.2x / SET ~1.3x faster"
  is measured: GET 1.148x, SET 1.272x, INCR 1.096x, HSET 1.379x. Reads dominate (clean, cv<5%):
  LRANGE 1.707x, SMEMBERS 1.846x, ZRANGE-WITHSCORES 1.275x, HGETALL 1.878x.
- **MEASURED LOSS — LPUSH ~0.54x (redis faster).** Re-measured 8 trials at depth 1 AND 16, both
  ~0.54 (cv 5.8 / 18). Real, not noise. ROOT = ChunkedList per-element Vec alloc on push
  (structural, bead 99fwc / project_list_restore_gap_architectural). NOT a recent lever — get/
  set/hset writes are all fr-faster, so it is list-specific. NO REVERT (nothing recent caused it);
  the fix is the packed-listpack-node ChunkedList rewrite (CoralOx). Retry: do not attempt to
  "revert a lever" for LPUSH — it is the known structural list gap, not a regression.
- **NO REVERTS this pass.** No backlog optimization showed a measured regression. The encode-path
  presize/direct-emit cluster + decode-string-move levers are byte-identical (gate-verified) so
  they cannot regress correctness; their throughput target (collection BGSAVE/MIGRATE/RELOAD) is
  NOT exercised by fr-bench (string-dump only) — needs a DEBUG-RELOAD-timing bench (owed).
- **METHOD constraint (Measured the hard way):** the full 36-cell matrix + heavy 2-server bench
  loops 144-KILL under cumulative sandbox load; only focused light batches (≤8 fr-bench runs,
  reused servers) complete. Publication-grade numbers still need a quiet host (bead vibu6).

## MEASURED cod-a set retain gauntlet (2026-06-19) — Criterion vs Redis 7.2.4

Scope: `frankenredis-gu5nf.32`, the stack-borrowed `SetValue::retain` decimal-byte
predicate candidate for mixed intset/generic set algebra. Harness:
`cargo bench -p fr-bench --bench set_algebra_vs_redis -- --noplot`, with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`,
`FR_SERVER_BIN=/data/projects/.rch-targets/frankenredis-cod-a/release/frankenredis`,
and the vendored Redis 7.2.4 oracle at
`/data/projects/frankenredis/legacy_redis_code/redis/src/redis-server`. The
workload builds realistic mixed sets (`small` 512 intset members, `large` 4096
generic decimal-byte members, `large_miss` 4096 disjoint members), then times
16-command batches of `SINTERSTORE`, `SDIFFSTORE`, and `SUNIONSTORE`.

Ratios below are command-throughput ratios. `fr/redis < 1` means Redis is faster.
The candidate row is the code-first stack-borrowed retain hunk; the reverted row
is the post-revert source state now kept on `main`.

| Workload | Candidate Redis cmds/s | Candidate fr cmds/s | Candidate fr/redis | Post-revert Redis cmds/s | Post-revert fr cmds/s | Post-revert fr/redis | Decision |
|---|--:|--:|--:|--:|--:|--:|---|
| SINTERSTORE mixed hit | 13,911 | 7,978 | 0.574 | 18,525 | 7,960 | 0.430 | REJECT: fr remains slower; no candidate gain |
| SDIFFSTORE mixed miss | 15,069 | 7,432 | 0.493 | 20,562 | 8,053 | 0.392 | REVERT: candidate was slower than reverted fr baseline |
| SUNIONSTORE mixed | 1,727 | 2,024 | 1.172 | 1,903 | 2,298 | 1.208 | existing fr win, unrelated to retain predicate |

Negative evidence:
- **No Redis-relative win on the target path:** mixed `SINTERSTORE` and `SDIFFSTORE`
  remain Redis-faster after the candidate, with fr at 0.574x and 0.493x respectively.
- **Candidate was not a same-frankenredis keep:** `SDIFFSTORE` improved from 7,432
  to 8,053 commands/s after reverting the stack-borrowed retain hunk (~8% better
  than the candidate on this run). `SINTERSTORE` was flat within noise.
- **`SUNIONSTORE` is not evidence for this lever:** it was already faster than Redis
  and does not exercise the retain-filter predicate that `gu5nf.32` changed.
- **Action taken:** source hunk removed manually; focused `fr-store` guard
  `rdb_and_ziplist_integer_restore_bytes_match_decimal_reference_edges` passed
  post-revert. A direct bench-binary rerun was used for the post-revert Criterion
  pass after the shared target dir hit a rustc-version cache mismatch; no target
  cleanup was performed.

## MEASURED cod-b exact keyed-write parser gauntlet (2026-06-19) — Criterion vs Redis 7.2.4

Scope: `frankenredis-bnrnp`, `frankenredis-2tbmh`, `frankenredis-8lqp4`,
`frankenredis-ons7i`, `frankenredis-r3on0`, `frankenredis-d061n`,
`frankenredis-unj78`, `frankenredis-nrybx`, `frankenredis-44wcq`,
`frankenredis-3gx3y`, `frankenredis-tp5aa`, `frankenredis-w0i5z`.

Harness: `cargo bench -p fr-bench --bench keyed_write_vs_redis -- --noplot`
with `FR_SERVER_BIN=/data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis`
and `REDIS_SERVER_BIN=/dp/frankenredis/legacy_redis_code/redis/src/redis-server`.
Current server was rch-built from `bf87bd00c`; pre-series A/B server was built from
`ecb5ca0a` (parent before the 5-value parser series). The benchmark sends canonical
single-byte-value `LPUSH`/`RPUSH`/`SADD` packets in 64-command pipelines; `FLUSHALL`
setup/cleanup is outside the Criterion timed section. Correctness gate:
`scripts/keyed_write_packet_differ.py 46791 46792` PASS — byte-exact vs Redis 7.2.4 for
LPUSH/RPUSH/SADD/ZADD N=4..19, HSET N=4..20, MSET fallback.

Ratios below are command-throughput ratios. `fr/redis < 1` means Redis is faster;
`current/pre` compares current frankenredis against the pre-5/16 parser baseline.

| Bead | Arity | LPUSH fr/redis | LPUSH current/pre | RPUSH fr/redis | RPUSH current/pre | SADD fr/redis | SADD current/pre | Decision |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| frankenredis-bnrnp | 5  | 0.409 | 1.019 | 0.800 | 1.173 | 0.847 | 1.100 | KEEP: RPUSH/SADD A/B wins; LPUSH still structural |
| frankenredis-2tbmh / 8lqp4 / ons7i | 8 | 0.350 | 1.036 | 0.802 | 1.142 | 1.161 | 1.223 | KEEP: SADD beats Redis; RPUSH A/B win |
| frankenredis-nrybx | 12 | 0.331 | 1.095 | 0.903 | 1.276 | 1.325 | 1.114 | KEEP: all three A/B wins; SADD beats Redis |
| frankenredis-w0i5z | 16 | 0.273 | 1.039 | 0.827 | 1.174 | 1.574 | 1.207 | KEEP: SADD beats Redis; RPUSH A/B win |

Per-bead rollup for the intermediate arities not shown individually in Criterion:
6/7/9/10/11/13/14/15 are part of the same generated exact-parser ladder and are covered
by the byte-exact differential gate; the measured arity sweep shows the ladder has real
positive signal on RPUSH/SADD, but does not close LPUSH. Do not extend this exact-arity
family solely to chase LPUSH — the list-push gap is `ChunkedList` structural allocation,
not parser dispatch.

Negative evidence:
- **LPUSH remains Redis-faster even when exact parsers fire**: 0.409x (5v), 0.350x (8v),
  0.331x (12v), 0.273x (16v). This is not a parser regression; current/pre is within
  noise to modestly positive, and the previous scorecard already tied LPUSH to the
  `ChunkedList` per-element push path. Retry only if a profiler attributes >=0.15% RPS p99
  to `parse_borrowed_plain_keyed_values*_packet` under an LPUSH/RPUSH/SADD arity-mix
  workload after the ChunkedList packed-node rewrite lands.
- **No revert this pass**: no arity showed a family-level regression versus the pre-series
  baseline; RPUSH improved 1.14-1.28x and SADD improved 1.10-1.22x on the sampled arities.

## MEASURED cod-b exact eight-key EXISTS parser gauntlet (2026-06-19) — Criterion vs Redis 7.2.4

Scope: `frankenredis-z3yrs`, the code-first exact canonical 8-key `EXISTS` packet parser.
Harness added in `fr-bench`: `cargo bench -p fr-bench --bench exists_vs_redis -- --noplot`.
The benchmark starts Redis 7.2.4 from `/dp/frankenredis/legacy_redis_code/redis/src/redis-server`
and a supplied `frankenredis` release binary, initializes `k0..k7`, then times 128-command
pipelines of `EXISTS` with all-hit, half-hit, and duplicate-key mixes. Setup and `FLUSHALL` are
outside the Criterion timed section. Ratios are command-throughput ratios; `fr/redis < 1` means
Redis is faster.

Decision-quality isolation used clean detached worktrees at `03709a07c`: clean `HEAD` and clean
`HEAD` with only the z3yrs eight-key parser branch/type/tests removed. Release servers were built
with rch into separate target dirs; the benchmark harness was compiled locally in separate target
dirs to avoid mixing rch-built `.rmeta` files with the local nightly.

| Workload | Redis cmds/s | fr HEAD cmds/s | fr/redis | fr no-z3yrs cmds/s | no-z3yrs/redis | HEAD/no-z3yrs | Decision |
|---|--:|--:|--:|--:|--:|--:|---|
| EXISTS 8 all hit | 1,124,940 | 866,759 | 0.770 | 776,600 | 0.708 | 1.116 | KEEP: z3yrs helps, but Redis still faster |
| EXISTS 8 half hit | 1,089,832 | 860,349 | 0.789 | 812,086 | 0.761 | 1.059 | KEEP: modest same-HEAD win |
| EXISTS 8 duplicates | 1,042,333 | 892,906 | 0.857 | 807,226 | 0.762 | 1.106 | KEEP: z3yrs helps, but Redis still faster |

Negative evidence:
- **Redis-relative loss remains:** clean `HEAD` is only 0.770x, 0.789x, and 0.857x Redis on the
  canonical 8-key `EXISTS` mixes. This is not a release-readiness win for the workload.
- **No revert:** removing only z3yrs made clean `HEAD` slower by 5.9-11.6%, so the exact parser is
  a measured same-HEAD keep, not a regression.
- **Confounded preliminary runs recorded, not used for the keep/revert decision:** the shared-dirty
  current binary measured 0.775x / 0.767x / 0.840x vs Redis, while the old parent `83544997b`
  measured 1.260x / 1.279x / 1.261x vs Redis. That comparison spans later unrelated commits and
  was treated as routing evidence only. It suggests a separate post-`83544997b` `EXISTS` slowdown
  profile is warranted, but it does not justify reverting z3yrs.
- **Correctness gates:** focused `cargo test -p fr-server borrowed_plain_exists_eight_packet`
  passed (2 parser tests). Full `fr-conformance` gate is recorded in the scorecard entry for this
  gauntlet.

Retry condition: do not extend the exact-arity `EXISTS` parser ladder from this evidence alone.
Retry only after a fresh profile on a quiet host names 8+ key `EXISTS` parser/dispatch as a top
hotspot, or after isolating the post-`83544997b` slowdown to a non-z3yrs commit.

## MEASURED cod-a quicklist2 PACKED RESTORE decode gauntlet (2026-06-19) — REJECTED

Scope: `frankenredis-ta8s1`, the code-first `fr-persist` QUICKLIST_2 PACKED decode change that
moved owned listpack string entries with `ListpackEntry::into_bytes()` instead of cloning through
`to_bytes()`. Harness added in `fr-bench`: `cargo bench -p fr-bench --bench
restore_quicklist_vs_redis`.

Workload: the harness starts Redis 7.2.4, builds a 96-member list with 40-byte members, reads the
Redis `DUMP` payload, asserts payload type 18 (`RDB_TYPE_LIST_QUICKLIST_2`), then times 8-command
pipelines of `RESTORE dst:N 0 <redis-dump-payload> REPLACE` against both Redis and FrankenRedis.
The timed path therefore decodes the exact original Redis 7.2.4 quicklist2 payload.

Measurement command: pinned rch worker `vmi1149989`; `frankenredis` release binary built with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`; benchmark invoked with
`REDIS_SERVER_BIN=/data/projects/frankenredis/legacy_redis_code/redis/src/redis-server` and
`FR_SERVER_BIN=/data/projects/frankenredis/.rch-target-vmi1149989-pool-92ff1a2a912611f45cd8f8e10ee25ce0/release/frankenredis`.
Earlier local/shared-target and `--noplot` attempts produced no measurements and are not evidence.

| Workload | Redis median cmds/s | fr candidate median cmds/s | fr/redis throughput | fr/redis time | Decision |
|---|--:|--:|--:|--:|---|
| QUICKLIST_2 PACKED RESTORE, 96x40B members | 236,900 | 87,777 | 0.371 | 2.699 | REJECT: Redis faster; source hunk reverted |

Negative evidence:
- The focused RESTORE gate contradicts the earlier broad DEBUG RELOAD read that looked favorable
  for list reloads; the isolated `ta8s1` decode-string move is not a Redis-relative win.
- Source hunk reverted to `entry.to_bytes()` for PACKED quicklist2 node decode.
- Keep the benchmark harness as the future gate before retrying this family.

## MEASURED cod-a quicklist2 RESTORE REPLACE slot reuse gauntlet (2026-06-19) — KEPT, residual Redis loss

Scope: `frankenredis-tnv37`, follow-up after the rejected owned-entry move above. The kept lever
changes `Store::restore_key_with_metadata` so `RESTORE ... REPLACE` overwrites an existing key's
entry in place instead of removing and reinserting the keyspace slot. It still clears old per-object
sidecars (`hash_field_ttl`, stream groups/last-id/entries-added/max-deleted-id) before installing
the new object.

Harness: `cargo bench -p fr-bench --bench restore_quicklist_vs_redis -- --noplot`. Release
`frankenredis` binaries were built with `rch exec -- env
CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo build --release -p
fr-server -p fr-bench`; the Criterion harness used
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a-localbench`, `FR_SERVER_BIN` from
the retrieved rch release binary, and Redis 7.2.4 at
`/data/projects/frankenredis/legacy_redis_code/redis/src/redis-server`.

| Workload | Redis median elems/s | fr median elems/s | fr/redis | fr candidate / no-candidate | Decision |
|---|--:|--:|--:|--:|---|
| QUICKLIST_2 PACKED RESTORE no-candidate | 112,860 | 49,455 | 0.438 | baseline | baseline |
| QUICKLIST_2 PACKED RESTORE in-place REPLACE | 117,310 | 52,584 | 0.448 | 1.063 | KEEP: +6.33% vs paired no-candidate |

Win/loss/neutral:
- Lever decisions in this `tnv37` pass: **1 win / 1 loss / 0 neutral**.
- Redis-relative score after the kept lever: **0 wins / 1 loss / 0 neutral**; Redis still wins this
  focused RESTORE workload by about 2.23x.

Negative evidence:
- **Rejected listpack-count preallocation**: preallocating decoded listpack vectors from the header
  count regressed the focused local Criterion run (`fr` median 52,335 elems/s vs the immediately
  prior 57,025 elems/s baseline) and was reverted. This is not a retry path unless a profiler names
  listpack vector growth directly.
- **Kept slot reuse does not close the target gap**: `fr/redis` improved only from 0.438 to 0.448.
  The remaining loss is deeper than keyspace remove/reinsert overhead.
- **Kernel profiling blocked** on this host (`perf_event_paranoid=4`), so timing proof is the
  acceptance evidence for this pass.

Correctness gates: focused `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a
cargo test -p fr-store restore_replace -- --nocapture` passed, including
`restore_replace_hash_clears_old_field_ttls` and
`restore_replace_stream_clears_old_consumer_groups`.

Retry condition: the next radical lever should attack generic RESTORE request materialization or
quicklist/listpack object construction, not the already-rejected `ListpackEntry::into_bytes` or
listpack count preallocation families.

## MEASURED cod-b boxed keyspace storage gauntlet (2026-06-19) — KEPT, residual gap open

Scope: `frankenredis-uhthd`, replacing the write-hot canonical keyspace key from
`Arc<[u8]>` to `Box<[u8]>` and keeping ordered/RANDOMKEY/volatile side views lazy. This applies
the graveyard/layout lever to the current measured RAM gap after earlier lazy ordered/random
index work: persistent keyspaces should not pay an Arc header/refcount for side indexes that are
not resident.

Build/proof bundle: `release-perf`, `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`.
`rch exec` builds/checks passed, but rch did not copy back the custom-target release-perf
executables; benchmark binaries were therefore materialized locally in the same target dir before
measurement. Raw artifacts:
`artifacts/optimization/frankenredis-uhthd-boxed-keys/20260619T0557Z/{baseline_memory.json,post_memory.json,throughput_smoke.txt,scan_invariant_gate.txt,summary.json}`.

Memory harness: `scripts/memory_baseline_capture.py`, fresh Redis 7.2.4 and FrankenRedis
processes, scale 200k, high non-colliding port bases.

| Cell | baseline fr/redis RSS | post fr/redis RSS | fr RSS delta | Redis-relative verdict |
|---|--:|--:|--:|---|
| keyspace | 1.688 | 1.348 | -5,935,104 B | KEEP: target gap shrank 20.1%, Redis still lighter |
| hash | 1.474 | 1.239 | -573,440 B | improves, still Redis-relative loss |
| list | 1.177 | 1.169 | -8,192 B | neutral/improves, still Redis-relative loss |
| set | 1.107 | 1.184 | -122,880 B | fr RSS improved; ratio worsened from Redis RSS variance |
| string_1k | 0.951 | 0.892 | -712,704 B | fr wins |
| stream | 0.981 | 0.978 | -4,784,128 B | fr wins |
| zset | 1.795 | 1.883 | -73,728 B | fr RSS improved; ratio worsened from Redis RSS variance |

Memory scorecard: Redis-relative win/loss/neutral **2/5/0** after the lever; FrankenRedis
absolute RSS improved in **7/7** cells. Target keyspace remains a Redis-relative loss at 1.348x,
so `uhthd` stays open for deeper dict/table compaction.

Throughput smoke: `bench_vs_redis.py` with `redis-benchmark`, 3 trials, 50k requests, p16/c50:
`SET 1.02x`, `GET 0.94x`, `HSET 1.06x`, `ZADD 0.84x`, `RANDOMKEY no data`
(`redis-benchmark` unsupported). With neutral band 0.90-1.00x, scorecard is **2/1/1**;
`ZADD` remains a measured Redis-relative gap and is not explained by this key-storage lever.

Correctness/gates: `cargo check --workspace --all-targets` PASS via rch; `cargo clippy
--workspace --all-targets -- -D warnings` PASS via rch; focused `fr-store` keyspace/volatile
tests PASS; `scan_invariant_gate.py` PASS; `cargo test -p fr-conformance -- --nocapture` PASS.
`cargo fmt --all -- --check` remains red on pre-existing formatting drift outside this lever
(`fr-command`, `fr-persist`, `fr-protocol`, `fr-server`, `fr-store/keyspace_dict.rs`,
`fr-store/packed_set.rs`); the production diff stayed scoped.

## MEASURED cod-b 8-key EXISTS encoded-reply gauntlet (2026-06-19) — KEPT, residual gap open

Scope: `frankenredis-upx5x`, the post-`frankenredis-z3yrs` 8-key `EXISTS` Redis-relative
slowdown. The kept lever adds a borrowed `_into` runtime path for `EXISTS key [key ...]` that
counts exactly as the existing borrowed path does, but writes the RESP integer reply directly to the
connection buffer (`:<count>\r\n`) and lets the server return `FastEncodedReply`. This removes
`RespFrame::Integer` materialization and the generic reply encoder from the exact 2-8 key parser
hot path and the generic borrowed-args fallback.

Build/proof bundle:
`artifacts/optimization/frankenredis-upx5x/20260619T1803Z/{control_original_exists_vs_redis_localbench.txt,candidate_exists_encoded_reply_localbench.txt,summary.json}`.
`rch exec` was attempted with `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`,
but the worker sync timed out and fail-open local execution hit mixed remote/local nightly metadata
in that shared target. The final release build and Criterion gate therefore used the compiler-scoped
subtarget `/data/projects/.rch-targets/frankenredis-cod-b/local-f20a92ec0`, still under the requested
root, with Redis 7.2.4 at `legacy_redis_code/redis/src/redis-server`.

| Workload | Control Redis elems/s | Control fr elems/s | Control fr/redis | Candidate Redis elems/s | Candidate fr elems/s | Candidate fr/redis | fr candidate/control | Decision |
|---|--:|--:|--:|--:|--:|--:|--:|---|
| EXISTS 8 all hit | 1,010,100 | 725,880 | 0.719 | 1,032,100 | 833,840 | 0.808 | 1.149 | KEEP: Criterion +10.794% fr throughput, p=0.00 |
| EXISTS 8 half hit | 917,040 | 703,800 | 0.768 | 1,085,200 | 871,830 | 0.803 | 1.239 | KEEP: Criterion +16.828% fr throughput, p=0.00 |
| EXISTS 8 duplicates | 897,910 | 704,600 | 0.785 | 1,037,100 | 927,980 | 0.895 | 1.317 | KEEP: Criterion +31.058% fr throughput, p=0.00 |

Win/loss/neutral:
- Lever decisions in this `upx5x` pass: **1 win / 2 losses / 1 neutral**.
- Redis-relative score after the kept lever: **0 wins / 3 losses / 0 neutral**. Redis still wins
  every focused `EXISTS` cell, but the residual gap narrowed materially.

Negative evidence and reverts:
- **Rejected enum arity-dispatch parser wrapper**: a single `EXISTS` packet enum dispatcher removed
  the parser cascade but regressed the local Criterion run and was removed before the keep.
- **Rejected 8-key-first parser reorder**: direct 8-key-first ordering improved over the enum wrapper
  but stayed mixed and did not cleanly beat the control across the scorecard; it was reverted.
- **Rejected `Store::drop_if_expired` no-expiry fast path**: skipping expiry-deadline lookup when no
  key TTLs exist looked promising, but the fair control/candidate run was neutral/noisy rather than
  a statistically clean keep, so it was reverted.
- **Kernel profiling blocked** on this host (`perf_event_paranoid=4`), so Criterion timing is the
  acceptance evidence for this pass.

Correctness gates: `cargo test -p fr-runtime plain_exists_borrowed -- --nocapture` PASS; `cargo
check -p fr-runtime -p fr-server --all-targets` PASS; `cargo clippy -p fr-runtime -p fr-server
--all-targets -- -D warnings` PASS; `cargo test -p fr-conformance -- --nocapture` PASS; `cargo fmt
--check` PASS. `ubs crates/fr-runtime/src/lib.rs crates/fr-server/src/main.rs` exited 1 on broad
pre-existing large-file findings; no finding was specific to the new encoded `EXISTS` path.

Retry condition: the next `EXISTS` pass should target the remaining Redis-relative loss in key
lookup/runtime accounting, not parser cascade order or no-expiry `drop_if_expired` micro-branches.

## MEASURED cod-b qk0nm residual EXISTS runtime-accounting pass (2026-06-19) — REJECTED

Scope: `frankenredis-qk0nm`, the residual 8-key `EXISTS` Redis-relative loss after the
`frankenredis-upx5x` encoded-reply keep. This pass tried runtime/accounting levers that were
distinct from the previously rejected parser cascade and single-key `drop_if_expired` no-expiry
micro-branch. All production hunks were reverted.

Build/proof bundle:
`artifacts/optimization/frankenredis-qk0nm/20260619T1842Z/{control_exists_small_integer_table_local_subtarget.txt,candidate_exists_small_integer_table_local_subtarget.txt,candidate_unrolled_exists_local_subtarget.txt,candidate_batch_exists_local_subtarget.txt,candidate_batch8_exists_local_subtarget.txt,summary.json}`.
`rch exec -- cargo build --release -p fr-server -p fr-bench` succeeded on worker `hz1`, but
`rch exec -- cargo bench -p fr-bench --bench exists_vs_redis -- --noplot` failed because the remote
bench process rewrote `FR_SERVER_BIN` to an ephemeral bench target without `release/frankenredis`.
The shared requested target then failed locally with mixed-nightly metadata (`654079540` artifacts
vs local `f20a92ec0`), so the measured A/B lane used fresh subtarget
`/data/projects/.rch-targets/frankenredis-cod-b/local-f20a92ec0-qk0nm` under the requested root.
Kernel profiling remained blocked by `perf_event_paranoid=4`.

Control (`HEAD` after the upx5x keep, with peer RESTORE sidecar diff present but no qk0nm code):

| Workload | Control Redis elems/s | Control fr elems/s | Control fr/redis |
|---|--:|--:|--:|
| EXISTS 8 all hit | 1,062,600 | 917,600 | 0.864 |
| EXISTS 8 half hit | 1,147,500 | 1,002,500 | 0.874 |
| EXISTS 8 duplicates | 1,222,100 | 932,190 | 0.763 |

Rejected candidates:

| Candidate | all-hit fr/redis | half-hit fr/redis | duplicate fr/redis | fr absolute vs control | Decision |
|---|--:|--:|--:|---:|---|
| Small pre-encoded integer reply table (`:0\r\n`..`:16\r\n`) + `_into` `u64` count | 0.754 | 0.812 | 0.839 | 0.848 / 0.795 / 0.854 | REJECT: significant fr throughput regression |
| Runtime exact-8 unrolled count over `exists_no_touch` | 0.777 | 0.755 | 0.769 | 0.841 / 0.733 / 0.817 | REJECT: significant fr throughput regression |
| `Store::exists_many_no_touch` batch helper with no-expiry aggregate hit/miss stats | 0.812 | 0.812 | 0.835 | 0.963 / 0.950 / 0.995 | REJECT: no credible same-control win; still Redis losses |
| Exact-8 specialization inside `exists_many_no_touch` | 0.789 | 0.807 | 0.822 | 0.853 / 0.823 / 0.857 | REJECT: significant fr throughput regression |

Win/loss/neutral:
- Lever decisions in this `qk0nm` pass: **0 wins / 4 losses / 0 neutral**.
- Redis-relative score after reverting: unchanged at **0 wins / 3 losses / 0 neutral** for the
  focused `EXISTS` suite.

Correctness gates while candidates were present: focused `fr-store exists_many_no_touch` tests PASS
for the batch helper; focused `fr-runtime plain_exists_borrowed` tests PASS for every candidate.
No production qk0nm code remained, so final validation is evidence-only plus source diff check.

Retry condition: do not retry small integer reply tables, exact-8 runtime unrolling, or batch
`exists_no_touch` stat aggregation for this workload. The next viable `EXISTS` route needs fresh
profile evidence naming a different primitive, likely command timing/histogram accounting,
connection write batching, or a larger keyspace-layout change shared with `frankenredis-uhthd`.

## MEASURED cod-a k263a quicklist2 RESTORE fused-stats pass (2026-06-19) — REJECTED

Scope: `frankenredis-k263a`, the remaining Redis-relative QUICKLIST_2 `RESTORE ... REPLACE`
materialization gap after the kept `frankenredis-tnv37` slot-reuse lever. This pass tried a
single production candidate: decode listpack value spans with raw/canonical encoded byte totals
and seed `ListValue` growth metadata from those totals instead of rebuilding through the
already-built quicklist chunks. The candidate preserved the prior canonical `lpBytes` rule and
passed focused tests, but did not improve the measured Redis-relative gate. The source hunk was
reverted.

Build and proof:
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo build --release -p fr-server -p fr-bench` PASS on worker `hz1`.
- Focused candidate guards while present: `rch exec -- ... cargo test -p fr-persist decode_value_spans_with_stats_matches_canonical_entry_sizing -- --nocapture` PASS; `rch exec -- ... cargo test -p fr-store restored_quicklist2_stats_constructor_matches_rescan -- --nocapture` PASS.
- Timing harness: `cargo bench -p fr-bench --bench restore_quicklist_vs_redis -- --noplot`, local same-host Criterion run using rch-built `frankenredis` and Redis 7.2.4 at `legacy_redis_code/redis/src/redis-server`.
- Kernel profiling was unavailable (`perf_event_paranoid=4`), so the accepted evidence is the focused Criterion A/B.

| Run | Redis elems/s | fr elems/s | fr/redis | Criterion verdict |
|---|--:|--:|--:|---|
| Control before candidate | 135.51 K | 56.476 K | 0.417 | baseline |
| Fused decode/growth stats candidate | 133.17 K | 55.544 K | 0.417 | no significant fr change; median -3.98% throughput, p=0.22 |

Win/loss/neutral:
- Lever decision in this `k263a` pass: **0 wins / 0 losses / 1 neutral**; no production hunk kept.
- Redis-relative score after reverting: unchanged at **0 wins / 1 loss / 0 neutral** for this
  focused QUICKLIST_2 RESTORE gate.

Retry condition: do not retry listpack-span stats fusion, post-build growth-state seeding, or
generic `lpBytes` rescan avoidance for QUICKLIST_2 RESTORE. The next viable `k263a` lever must
target request materialization/key-payload cloning in the runtime/server path, direct quicklist
object construction, or a fresh profile-named primitive distinct from listpack span accounting.

## MEASURED cod-a h6ppr RESP CRLF memchr scanner pass (2026-06-19) — REJECTED

Scope: `frankenredis-h6ppr`, a code-first `fr-protocol::read_line` candidate that replaced the
byte-by-byte CRLF search with `memchr::memchr`. This targeted hot-path RESP command parsing while
avoiding peer-owned runtime/keyspace/persistence surfaces. Focused parser guards passed when the
candidate was present, but release A/B did not justify carrying the production hunk. The source
hunk and direct `fr-protocol` `memchr` dependency were reverted.

Build and proof:
- Current release build: `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo build --release -p fr-server -p fr-bench` PASS on worker `hz2`.
- Control release build: detached worktree `/data/projects/frankenredis-h6ppr-control`, current
  `HEAD` with only the h6ppr scanner patch reversed, built via
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a-h6ppr-control cargo build --release -p fr-server -p fr-bench` PASS on worker `hz2`.
- Kernel profiling unavailable: `/proc/sys/kernel/perf_event_paranoid` was `4`.
- Timing artifacts:
  `artifacts/optimization/frankenredis-h6ppr/verify_memchr_crlf_20260619T234447Z/summary.json`,
  `.../confirm_long_current_control/summary.json`, and
  `.../confirm_p128_2m/summary.json`.

Initial Redis-relative run (fresh server per engine, 300k requests/cell, 5 trials/cell) showed
FrankenRedis faster than Redis 7.2.4 in all four GET/SET cells, but the current/control cells were
too noisy (`cv_pct > 5`) for a keep decision:

| Workload | Redis ops/s | Control ops/s | Current ops/s | current/control | current/redis |
|---|--:|--:|--:|--:|--:|
| GET P16 | 939,666 | 1,186,062 | 1,208,901 | 1.019 | 1.287 |
| SET P16 | 856,098 | 1,038,795 | 1,033,280 | 0.995 | 1.207 |
| GET P128 | 2,257,312 | 3,023,477 | 2,993,988 | 0.990 | 1.326 |
| SET P128 | 1,869,422 | 2,725,010 | 2,664,623 | 0.978 | 1.425 |

Longer current-vs-control confirmation (1M requests/cell, interleaved by workload, 5 trials/cell):

| Workload | Control ops/s | Control cv | Current ops/s | Current cv | current/control | Verdict |
|---|--:|--:|--:|--:|--:|---|
| GET P16 | 1,103,074 | 3.23% | 1,102,414 | 3.24% | 0.999 | neutral |
| SET P16 | 992,467 | 3.21% | 1,010,799 | 1.38% | 1.018 | win |
| GET P128 | 3,305,999 | 5.88% | 3,478,986 | 2.44% | 1.052 | noisy win |
| SET P128 | 2,777,399 | 2.98% | 2,934,994 | 5.87% | 1.057 | noisy win |

Deeper P128 confirmation (2M requests/cell, 5 trials/cell) reversed the noisy P128 signal:

| Workload | Control ops/s | Control cv | Current ops/s | Current cv | current/control | Verdict |
|---|--:|--:|--:|--:|--:|---|
| GET P128 | 3,635,483 | 2.55% | 3,486,702 | 2.68% | 0.959 | loss |
| SET P128 | 2,919,684 | 4.77% | 2,913,354 | 3.01% | 0.998 | neutral |

Win/loss/neutral:
- Lever decision for `h6ppr`: **1 win / 1 loss / 2 neutral** on the low-CV confirmation set
  (`SET P16` win, `GET P128` loss, `GET P16` and `SET P128` neutral). Because the loss is clean
  and the only clean win is small, no production hunk is kept.
- Redis-relative score after reverting: unchanged favorable for this focused GET/SET harness;
  the first-pass control ratios were **4 wins / 0 losses / 0 neutral** vs Redis 7.2.4, but this
  pass does not claim h6ppr as the cause.

Retry condition: do not retry generic CRLF `memchr` scanning, byte-by-byte line-scan rewrites, or
line scanner micro-specialization unless a fresh profile names `fr-protocol::read_line` or CRLF
search self-time as a dominant parser cost and a low-CV current-vs-control bench shows no P128
regression.

## MEASURED cod-b uhthd inline-small StoreKey pass (2026-06-20) — REJECTED

Scope: `frankenredis-uhthd`, a keyspace-RAM experiment replacing boxed key storage with an enum
that inlined keys up to 15 bytes and heap-boxed longer keys. This was the arena/exotic-layout
angle from the keyspace dict RAM gap: remove the small-key heap allocation for Redis-benchmark-like
short keys without changing SCAN/RANDOMKEY behavior. Focused `fr-store` guards passed while the
candidate was present, but release RSS head-to-head against Redis 7.2.4 regressed too many memory
cells. The source hunk was reverted; no production code from this lever shipped.

Build and proof:
- Baseline/reverted builds:
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec -- cargo build --release -p fr-server -p fr-bench`.
- Memory harness: `scripts/memory_baseline_capture.py` against vendored Redis 7.2.4
  (`legacy_redis_code/redis/src/redis-server`) and the rebuilt FrankenRedis release binary,
  scale 200k, high non-colliding ports.
- Correctness gates after reverting:
  `cargo fmt --check`, `rch exec -- cargo check --workspace --all-targets`,
  `rch exec -- cargo clippy --workspace --all-targets -- -D warnings`, and
  `rch exec -- cargo test -p fr-conformance -- --nocapture` all passed.
- Proof bundle:
  `artifacts/optimization/frankenredis-uhthd-smallkey/20260620T0001Z/summary.json`.

| memory cell | baseline fr/redis RSS | candidate fr/redis RSS | candidate fr RSS delta | reverted-control fr/redis RSS |
|---|---:|---:|---:|---:|
| keyspace | 1.169 | 1.465 | +2,883,584 B | 1.246 |
| string_1k | 0.879 | 0.894 | +90,112 B | 0.893 |
| list | 1.186 | 1.399 | +90,112 B | 1.206 |
| hash | 1.392 | 1.410 | +208,896 B | 1.375 |
| set | 1.075 | 1.243 | +294,912 B | 1.222 |
| zset | 1.834 | 1.579 | -405,504 B | 1.720 |
| stream | 0.974 | 0.977 | +585,728 B | 0.979 |

Win/loss/neutral:
- Lever absolute FrankenRedis RSS score vs same-run baseline: **1 win / 6 losses / 0 neutral**.
  The only absolute RSS win was zset, while the target keyspace cell regressed by 25.3%
  Redis-relative and about 2.9 MB absolute.
- Redis-relative score after the rejected candidate: **2 wins / 5 losses / 0 neutral**; unchanged
  count, worse target cell.
- Reverted-control score after rebuilding from reverted source: **2 wins / 5 losses / 0 neutral**,
  RSS geomean `1.210x`; `uhthd` remains open because keyspace RSS is still heavier than Redis.

Retry condition: do not retry inline-small-key enums, tagged key wrappers, or per-entry key
inlining in the current `HashMap<StoreKey, Entry>` shape unless a new layout proof shows the table
entry does not grow and a fresh same-worker memory gate improves the keyspace cell. The next viable
`uhthd` lever needs a deeper keyspace-dict representation change: lower table metadata, split
fingerprints/keys, or a SCAN/RANDOMKEY design-level tradeoff with explicit semantics review.

## MEASURED cod-b ohsk5 cached borrowed write gate (2026-06-20) -- KEEP, residual losses remain

Scope: `frankenredis-ohsk5`, verification of previously coded commit
`d14e2b330` ("cached borrowed write gate, code-first batch-test pending"). The lever caches the
default borrowed-write predicate once per buffered multibulk batch for exact SET/MSET/HSET fast
paths instead of rescanning auth/ACL/session/server state for every borrowed write command. The
inverse-control worktree was current `HEAD` with only `d14e2b330` reverted; no production source
was changed in this pass.

Build and proof:
- Current build:
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec -- cargo build --release -p fr-server -p fr-bench`.
- Inverse-control build:
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b-ohsk5-control rch exec -- cargo build --release -p fr-server -p fr-bench`.
- Oracle: vendored Redis 7.2.4, `legacy_redis_code/redis/src/redis-server` and
  `legacy_redis_code/redis/src/redis-benchmark`, both reporting 7.2.4.
- Proof bundle:
  `artifacts/optimization/frankenredis-ohsk5-cached-write-gate/20260620T015044Z/`.
- Correctness gate:
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec -- cargo test -p fr-conformance -- --nocapture`
  passed: lib tests, bin tests, smoke tests, and doctests all green.

Focused current-vs-inverse-control `fr-bench` gate (P16/c50/n300k, 5 trials):

| workload | control ops/s | control cv | current ops/s | current cv | current/control | verdict |
|---|---:|---:|---:|---:|---:|---|
| SET P16 | 983,672 | 4.00% | 1,098,604 | 2.93% | 1.117 | keep-grade win |
| HSET P16 | 858,384 | 3.04% | 908,586 | 8.92% | 1.058 | noisy support only |

`redis-benchmark` current-vs-inverse-control (P16/c50/n150k, 7 interleaved trials) did not show a
clean multi-command speedup: SET 1.05x median, HSET 0.99x, MSET 1.01x. This does not invalidate the
clean SET `fr-bench` win, but it limits the claim.

Current HEAD vs Redis 7.2.4, same `redis-benchmark` gate:

| command | fr/redis median | trials | verdict |
|---|---:|---|---|
| SET | 1.02x | 0.93, 1.12, 0.97, 1.07, 1.04, 1.02, 0.87 | neutral by 3% band |
| HSET | 0.95x | 1.12, 0.84, 0.89, 0.95, 1.17, 1.14, 0.94 | Redis-relative loss |
| MSET | 0.96x | 1.01, 0.95, 0.96, 0.93, 0.90, 1.03, 1.13 | Redis-relative loss |

Win/loss/neutral:
- Lever keep gate vs inverse control: **1 keep-grade win / 0 keep-grade losses / 1 noisy support**.
  The SET P16 gain is clean and above the keep threshold; the HSET gain is not clean enough to
  claim as a keeper by itself.
- Narrow Redis-relative command-family score: **0 wins / 2 losses / 1 neutral** by a 3% band.
  All three commands remain at or above the project parity floor used by `bench_vs_redis.py`
  (median ratio >= 0.9x), but HSET/MSET are still not dominating Redis.
- Broad quick `.bench-history` score against Redis 7.2.4:
  **22 wins / 15 losses / 2 neutral** across all 39 cells, but **34/39 cells were noisy** under
  the 5% CV rule. Stable cells only: **3 wins / 2 losses / 0 neutral** (`GET@P1`,
  `INTEGER-GET@P1`, `SET@P1` wins; `INCR@P1`, `MIXED@P1` losses).

Decision: keep the existing cached borrowed write gate; the original pending benchmark proof is now
complete. Do not claim broad domination from this pass. Next `ohsk5` work should target measured
stable losses, especially `MIXED@P1` and `INCR@P1`, or rerun the noisy P16/P128 gaps on a quieter
worker before spending code on them.

## MEASURED cod-b ohsk5 HSET direct histogram candidate (2026-06-20) -- REJECTED

Scope: `frankenredis-ohsk5`. Candidate added a dedicated `hset: Option<CommandHistogram>` field to
`CommandHistogramTracker` so HSET commandstats/latency recording could avoid the fallback
`HashMap<String, CommandHistogram>` lookup. The idea was deliberately small and branch-local, but
the same-binary A/B did not clear the keep bar, so the source hunk was reverted.

Build and proof:
- Baseline binary: pre-candidate `frankenredis-baseline`, sha256
  `e16617e886d70d1ca22873a511ebd25d725e650716deeca7827cfadd342380cd`.
- Candidate binary: HSET-direct-hist build, sha256
  `46e3c55dad16a63ee165a0bd81ce883d19bce37f2b6a2c3e8a90fd2b9f1d1b7c`.
- Clean-source rebuild after revert:
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec -- cargo build --release -p fr-server -p fr-bench`
  passed on `vmi1149989`; rebuilt `frankenredis` sha256
  `10ce6936071c04ca41dcba795cc7261a6a1b59c21c62c4543edd5e6242903880`.
- Proof bundle:
  `artifacts/optimization/frankenredis-ohsk5-hset-direct-hist/20260620T022647Z/`.
- Profiling note: kernel sampling was blocked by `/proc/sys/kernel/perf_event_paranoid=4`, so this
  closeout uses release A/B timing and harness CV gates rather than `perf` stacks.

HSET direct-hist A/B, same host/server pair, `fr-bench --workload hset`, c4, n300k, 7 trials:

| depth | order | baseline ops/s | candidate ops/s | candidate/baseline | baseline cv | candidate cv | verdict |
|---|---|---:|---:|---:|---:|---:|---|
| p1 | baseline-first | 93,555 | 92,915 | 0.993 | 3.75% | 2.81% | neutral/slight down |
| p1 | candidate-first | 91,603 | 90,966 | 0.993 | 2.08% | 3.76% | neutral/slight down |
| p16 | baseline-first | 840,254 | 938,223 | 1.117 | 6.42% | 4.87% | noisy |
| p16 | candidate-first | 686,454 | 884,237 | 1.288 | 8.12% | 14.57% | noisy |
| p128 | baseline-first | 1,821,120 | 2,004,639 | 1.101 | 11.67% | 9.34% | noisy |
| p128 | candidate-first | 1,580,185 | 1,636,568 | 1.036 | 15.46% | 3.91% | noisy |

Lever score: **0 wins / 0 losses / 2 neutral-clean / 4 noisy**. P1 is the only clean depth and it
is slightly down, while P16/P128 are not publishable because at least one side exceeds the 5% CV
noise gate. Retry condition: do not add more per-command histogram direct fields unless a fresh
profile names commandstats accounting and a paired A/B shows a clean same-control win at P1.

Focused current HEAD vs Redis 7.2.4 after reverting, same clean-source binary above, `fr-bench`
c4, 7 trials:

| cell | Redis ops/s | fr ops/s | fr/redis | Redis cv | fr cv | verdict |
|---|---:|---:|---:|---:|---:|---|
| `mixed@p1` | 97,244 | 100,276 | 1.031 | 2.21% | 5.69% | noisy, not a clean loss |
| `mixed@p16` | 879,203 | 1,068,279 | 1.215 | 8.09% | 9.23% | noisy |
| `incr@p1` | 98,241 | 93,771 | 0.954 | 3.55% | 3.39% | clean Redis-relative loss |
| `incr@p16` | 824,907 | 943,306 | 1.144 | 6.41% | 9.14% | noisy |
| `get@p1` | 98,551 | 101,903 | 1.034 | 2.81% | 3.29% | clean win |
| `set@p1` | 98,132 | 97,462 | 0.993 | 2.86% | 4.86% | neutral |
| `hset@p1` | 94,892 | 94,396 | 0.995 | 3.06% | 4.63% | neutral |
| `hset@p16` | 964,076 | 1,030,433 | 1.069 | 6.17% | 4.45% | noisy |
| `hset@p128` | 1,887,664 | 2,217,628 | 1.175 | 6.02% | 7.02% | noisy |

Focused Redis-relative score after reverting: **1 win / 1 loss / 2 neutral / 5 noisy** across all
nine cells; clean cells only: **1 win / 1 loss / 2 neutral**. `INCR@P1` remains the clean target.
`MIXED@P1` is downgraded from "stable loss" to noisy/rerun-required for this specific focused gate.

Correctness gates after reverting:
- `cargo fmt --check` passed.
- `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec -- cargo test -p fr-conformance -- --nocapture`
  passed: 194 lib tests, conformance bins, 99 smoke tests, and doctests green.

## MEASURED cod-b 15lug residual CV confirmation + missing-key expiry candidate (2026-06-20) -- CANDIDATE REJECTED

Scope: `frankenredis-15lug`. This pass first ran the project ratcheted `fr-bench` matrix against
vendored Redis 7.2.4, then confirmed the pass195 residual commands with vendored
`redis-benchmark`. The code lever tested after the focused sweep was deliberately small: return
from `Store::drop_if_expired` immediately when `entries.get(key)` is absent, avoiding an
`expiry_deadlines` probe on missing-key write-pop commands such as benchmark `SPOP myset`.

Official `.bench-history` matrix:
- Command:
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b python3 scripts/perf_baseline_capture.py legacy_redis_code/redis/src/redis-server /data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis /data/projects/.rch-targets/frankenredis-cod-b/release/fr-bench --trials 7`.
- Result: ratchet **FAIL**, but baseline was captured to `.bench-history/comprehensive_bench.latest.json`.
- Stable score: **7 wins / 6 losses / 2 neutral**, with 23 noisy cells and `mixed@p128` skipped.
- Clean losses: `dump@p128=0.375x`, `mixed@p16=0.347x`, `dump@p1=0.716x`,
  `lpush@p1=0.806x`, `hget@p1=0.937x`, `incr@p1=0.959x`.
- Ratchet regressions vs prior baseline: `integer-get@p1 -9.9%`, `lpush@p1 -7.5%`,
  `dump@p1 -6.0%`, `dump@p128 -18.5%`, `mixed@p16 -72.2%`.

Focused pass195 residual sweep, current HEAD before the rejected candidate:
- Artifact:
  `artifacts/optimization/frankenredis-15lug-cv-confirm/20260620T042556Z/redis_benchmark_p16_c50_n150k_trials7.txt`.
- Harness: vendored `redis-benchmark`, P16, c50, n150k, 7 interleaved trials.

| command | median fr/redis | trials | verdict |
|---|---:|---|---|
| incr | 1.12 | 1.08, 1.18, 1.12, 1.15, 1.08, 1.11, 1.17 | win |
| lpush | 0.91 | 0.91, 0.93, 0.89, 0.98, 0.87, 0.96, 0.80 | neutral |
| rpush | 1.03 | 1.29, 0.91, 1.18, 1.03, 1.00, 0.98, 1.06 | win |
| spop | 0.81 | 0.81, 0.75, 0.85, 0.76, 0.78, 0.93, 0.90 | loss |
| lrange_100 | 1.08 | 1.31, 1.26, 1.08, 1.02, 0.88, 1.13, 1.02 | win |
| lrange_500 | 1.24 | 1.21, 1.18, 0.73, 1.31, 1.25, 1.32, 1.24 | win |
| lrange_600 | 1.15 | 1.15, 1.14, 1.17, 1.43, 1.03, 1.02, 1.47 | win |
| ping_inline | 1.01 | 0.80, 1.16, 1.14, 1.06, 0.92, 1.01, 0.86 | neutral |
| ping_mbulk | 0.93 | 0.82, 0.79, 0.94, 1.03, 0.93, 0.86, 0.96 | neutral |

Focused score by 3% band: **5 wins / 1 loss / 3 neutral**. Only `spop` is below the 0.9x parity
floor from the old pass195 residual list; `lrange_500`, `rpush`, `incr`, and `ping_mbulk` are not
confirmed as chase targets on this focused gate.

Rejected candidate sweep:
- Artifact:
  `artifacts/optimization/frankenredis-15lug-cv-confirm/20260620T043401Z-candidate/redis_benchmark_p16_c50_n150k_trials7.txt`.
- Candidate build:
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec -- cargo build --release -p fr-server -p fr-bench`.
- Result: `spop` remained **0.81x**; `lpush` fell to **0.77x** and `rpush` to **0.88x** in the
  focused sweep. The candidate hunk was reverted before committing.

Decision: do not ship the missing-key expiry-map short-circuit. Next code work should target the
actual `SPOP` nil/write-pop runtime shape with profiling or a same-worker current-vs-control gate,
not generic expiry lookup pruning.

## MEASURED cod-a 15lug.1 SPOP exact-parser ordering (2026-06-20) -- CANDIDATE KEPT

Scope: `frankenredis-15lug.1`. This pass targeted the remaining vendored Redis 7.2.4
`redis-benchmark -t spop` loss from the prior 15lug residual sweep. The kept lever is in
`crates/fr-server/src/main.rs`: accept no-count `SPOP key` in the exact keyed-pop packet parser,
then try that keyed-pop parser immediately after the exact `GET` parser instead of after the long
keyed-values and miscellaneous exact-parser ladder. This preserves the same borrowed runtime/store
SPOP implementation and leaves `SPOP key count` on the generic path.

Fresh baseline before code changes:
- Artifact: `artifacts/optimization/frankenredis-15lug-1/20260620T053608Z-baseline/bench_vs_redis_p16_c50_n150k_trials7.txt`.
- Harness: vendored `redis-benchmark`, P16, c50, n150k, 7 interleaved trials.

| command | median fr/redis | trials | verdict |
|---|---:|---|---|
| spop | 0.75 | 0.77, 0.73, 0.77, 0.70, 0.75, 0.74, 0.78 | loss |
| lpush | 0.78 | 0.78, 0.79, 0.76, 0.76, 0.79, 0.80, 0.77 | loss |
| rpush | 0.91 | 0.81, 0.92, 0.93, 0.91, 0.88, 0.84, 0.91 | neutral |

First candidate, exact-parser inclusion only:
- Artifact: `artifacts/optimization/frankenredis-15lug-1/20260620T053837Z-spop-exact-parser-candidate/bench_vs_redis_p16_c50_n150k_trials7.txt`.

| command | median fr/redis | trials | verdict |
|---|---:|---|---|
| spop | 0.86 | 0.80, 0.82, 0.85, 0.86, 0.94, 0.94, 0.93 | improved but still below 0.9x |
| lpush | 0.78 | 0.78, 0.89, 0.93, 0.70, 0.78, 0.78, 0.72 | loss |
| rpush | 0.91 | 0.97, 0.84, 0.94, 0.94, 0.91, 0.90, 0.86 | neutral |

Same-host control/candidate A/B:
- Artifact: `artifacts/optimization/frankenredis-15lug-1/20260620T054137Z-control-candidate-ab/summary.txt`.
- Counted runs: control 1, candidate 2, candidate 3, control 5.
- Invalid runs: control 4 and 4b were discarded because Redis failed to bind the selected port;
  no throughput result was counted from those launches.

| variant | command | median fr/redis | verdict |
|---|---|---:|---|
| control 1 | spop | 0.75 | loss |
| control 1 | lpush | 0.79 | loss |
| control 1 | rpush | 0.82 | loss |
| candidate 2 | spop | 0.83 | improved but still below 0.9x |
| candidate 2 | lpush | 0.76 | loss |
| candidate 2 | rpush | 0.89 | loss |
| candidate 3 | spop | 0.93 | win vs parity floor |
| candidate 3 | lpush | 0.76 | loss |
| candidate 3 | rpush | 0.89 | loss |
| control 5 | spop | 0.68 | loss |
| control 5 | lpush | 0.84 | loss |
| control 5 | rpush | 0.93 | neutral |

Profile after exact-parser inclusion:
- Command: `AGENT_NAME=cod-a CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a scripts/profile_hot_path.sh -t spop -P 16 -n 2000000 -c 50 -s 6 -r 100000`.
- Perf data: `/data/tmp/claude-1000/profile_hot_path_4149131.data`.
- Throughput during profile: `799680.12 requests per second`.
- Hotspots: `process_buffered_frames` 12.03% self / 22.16% inclusive,
  `execute_plain_keyed_pop_borrowed` 1.58% self / 5.47% inclusive,
  `parse_borrowed_multibulk_action` 1.93% self / 3.40% inclusive,
  `parse_command_args_borrowed_into` 1.45% self / 2.32% inclusive, and failed exact-parser probes
  such as `parse_borrowed_plain_echo_packet`, `parse_borrowed_plain_xlen_packet`, and
  `parse_borrowed_plain_keyed_values10_packet`. This routed the second lever toward parser ordering
  rather than store data-structure changes.

Kept combined candidate, exact-parser inclusion plus early keyed-pop ordering:
- Artifact: `artifacts/optimization/frankenredis-15lug-1/20260620T054808Z-early-keyed-pop-candidate/bench_vs_redis_p16_c50_n150k_trials7.txt`.

| command | median fr/redis | trials | verdict |
|---|---:|---|---|
| spop | 1.03 | 1.07, 0.86, 1.02, 1.04, 1.05, 0.93, 1.03 | win |
| lpop | 1.02 | 0.89, 1.08, 1.02, 1.00, 1.04, 1.34, 0.95 | win |
| rpop | 1.00 | 1.05, 1.12, 0.81, 0.88, 1.01, 0.98, 1.00 | neutral |
| lpush | 0.75 | 0.71, 0.85, 0.73, 0.86, 0.68, 0.75, 0.87 | loss |
| rpush | 0.91 | 0.87, 0.87, 0.91, 1.06, 0.97, 0.85, 0.91 | neutral |

Confirmation run:
- Artifact: `artifacts/optimization/frankenredis-15lug-1/20260620T054843Z-early-keyed-pop-confirm/bench_vs_redis_p16_c50_n150k_trials7.txt`.

| command | median fr/redis | trials | verdict |
|---|---:|---|---|
| spop | 1.04 | 0.93, 1.04, 0.90, 1.06, 0.89, 1.11, 1.15 | confirmed win |
| lpush | 0.78 | 0.76, 0.80, 0.80, 0.78, 0.82, 0.77, 0.74 | residual loss |
| rpush | 0.89 | 0.99, 0.85, 0.90, 0.89, 0.87, 0.92, 0.86 | residual loss/noisy floor |

Decision: keep the exact keyed-pop SPOP parser plus early keyed-pop parser ordering. The focused
SPOP residual moved from a fresh 0.75x baseline and prior 0.81x residual confirmation to 1.03x and
1.04x Redis-relative medians. Do not treat this as a list-push fix: `LPUSH` remains below the 0.9x
floor in every cod-a focused run, and `RPUSH` is noisy around the floor. Next target should be the
list push path, not another SPOP parser lever.

## MEASURED cod-b fresh-restart 15lug.1 SPOP front-loaded keyed-pop route (2026-06-20) -- CANDIDATE KEPT

Scope: `frankenredis-15lug.1`. This fresh restart re-verified the SPOP lane under
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b` and vendored Redis 7.2.4
`redis-benchmark`, P16, c50, n150k. The first attempted lever only added SPOP to the existing
late exact keyed-pop packet parser; it was rejected. The kept lever moves the no-count keyed-pop
exact parser up to just after the PING/GET exact parsers and before the high-arity keyed-value
writer ladder, and includes exact `SPOP key` recognition there. `SPOP key count` remains on the
generic path.

Baseline before cod-b changes:
- Artifact:
  `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T053450Z-baseline/current_vs_redis_redis_benchmark.txt`.
- Result: `spop` median **0.77x** vs Redis 7.2.4; `lpush` **0.77x**, `rpush` **0.86x**.

Rejected exact-packet-only candidate:
- Same-current artifact:
  `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T054210Z-candidate-control/candidate_vs_control_redis_benchmark.txt`.
- Redis artifact:
  `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T054238Z-candidate-redis/candidate_vs_redis_redis_benchmark.txt`.
- Result: SPOP improved only **1.02x** vs current-control and stayed **0.78x** vs Redis; source hunk
  was reverted before the second candidate.

Profile route:
- Artifact:
  `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T054407Z-profile-current-spop/perf_report_no_children.txt`.
- Top self samples: `process_buffered_frames` **14.01%**, `parse_command_args_borrowed_into`
  **1.85%**, `execute_plain_keyed_pop_borrowed` **1.71%**,
  `plain_borrowed_default_key_write_allows` **1.52%**, `parse_borrowed_multibulk_action`
  **1.24%**, and `Store::spop` only **0.38%**. This rejected set-storage tinkering and routed the
  kept lever toward parser ordering.

Kept final candidate:
- Five-command guard artifact:
  `artifacts/optimization/frankenredis-15lug-spop-frontload-pop/20260620T055254Z-final-five-command/`.
- SPOP-focused confirmation artifact:
  `artifacts/optimization/frankenredis-15lug-spop-frontload-pop/20260620T055340Z-final-spop-focused/`.

| gate | command | median ratio | trials | verdict |
|---|---|---:|---|---|
| final/current-control | spop | 1.25 | 7 | keep |
| final/current-control | lpop | 1.11 | 7 | guard win |
| final/current-control | rpop | 1.08 | 7 | guard win |
| final/current-control | lpush | 1.00 | 7 | no regression |
| final/current-control | rpush | 1.04 | 7 | no regression |
| final/Redis 7.2.4 | spop | 1.06 | 7 | SPOP floor cleared |
| final/Redis 7.2.4 | lpop | 1.03 | 7 | parity/win |
| final/Redis 7.2.4 | rpop | 1.01 | 7 | parity/win |
| final/Redis 7.2.4 | lpush | 0.83 | 7 | residual list-write loss |
| final/Redis 7.2.4 | rpush | 0.85 | 7 | residual list-write loss |
| SPOP-focused final/current-control | spop | 1.30 | 11 | confirmed keep |
| SPOP-focused final/Redis 7.2.4 | spop | 1.00 | 11 | confirmed parity |

Decision: keep the front-loaded no-count keyed-pop exact route. Do not retry the exact-packet-only
SPOP addition; it was too small and still below Redis parity. The next measured gaps in this family
are list writes (`LPUSH`/`RPUSH`), not SPOP storage or parser reshuffling.

## MEASURED cod-a zset DUMP score-entry shortcut rejection (2026-06-20) -- NO SOURCE KEPT BY COD-A

Scope: `frankenredis-zset-listpack-score-zero-copy-z56kl` evidence lane plus dirty
`fr-store` candidate marked `frankenredis-dump-zset-score-int`. The targeted loss
was `fr-bench --workload dump` at 50 clients, pipeline 128, keyspace 10000, where
the workload preloads compact int-scored zsets and then times `DUMP`.

Profile route:
- BlackThrush's shared `dump@p128` sample reported FrankenRedis at roughly
  `153k ops/s` vs Redis `366k ops/s` (`0.42x`) and attributed server self-time
  to `lzf`, `Store::dump_key`, `encode_listpack_entry`, score formatting/reparse,
  and CRC.
- Local kernel `perf` was blocked for cod-a by `perf_event_paranoid=4`.
- `scripts/profile_hot_path.sh` was not used as a proof path for this workload
  because it drives vendored `redis-benchmark`; the zset-prefilled DUMP workload
  is custom `fr-bench`.

Baseline and A/B evidence:

| artifact | gate | ratio | cv | verdict |
|---|---|---:|---|---|
| `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T061700Z-baseline/summary.txt` | current/control vs Redis 7.2.4 | 0.616569 fr/redis | redis 5.27%, fr 3.13% | routing loss, Redis side slightly noisy |
| `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T062635Z-dirty-candidate-ab/summary.txt` | dirty candidate vs saved control | 1.080504 candidate/control | control 4.73%, candidate 4.96% | supporting win only |
| same | dirty candidate vs Redis 7.2.4 | 0.569797 candidate/redis | redis 16.78% | Redis leg too noisy for keep claim |
| `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T062741Z-candidate-control-confirm/summary.txt` | dirty candidate vs saved control, 500k requests, 9 trials | 0.955895 candidate/control | control 3.71%, candidate 2.38% | rejected current form |

Correctness guard:
`AGENT_NAME=cod-a CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a rch exec -- cargo test -p fr-store zset_score_int_listpack_fastpath_is_byte_identical_to_string_form -- --nocapture`
passed; byte identity was not the rejection reason.

Decision: no cod-a source keep. The active dirty `crates/fr-store/src/lib.rs`
hunk was under BlackThrush's exclusive reservation, so cod-a did not stage,
commit, or revert it. The measured result says the micro-shortcut is not enough:
current-form score-integer direct encoding regressed the stronger confirmation
gate and leaves DUMP well below Redis. Next DUMP attempt should attack a deeper
representation, such as retaining/caching the compact zset listpack payload or a
single-pass DUMP-side representation that avoids rebuilding from `IndexMap` plus
`BTreeMap` for every DUMP. Do not add more score-formatting shortcuts unless a
fresh profile names them and a same-current A/B stays positive.

## ZLEXCOUNT store-side micro-opt — DECLINED on measurement (BlackThrush 2026-06-20)

Surfaced by an extended compute-heavy differential probe (reusing one live
Redis 7.2.4 + shipped fr): `ZLEXCOUNT` was the only fresh standout loss
(`0.24x` on a *varying-score* zset; `0.64–0.80x` on the realistic *equal-score*
zset across bounded + full ranges). fr DOMINATES the rest of the extended set
(`zintercard 12.5x`, `zinterstore 2.05x`, `zunionstore 1.75x`, `zdiffstore
1.49x`, `sort_limit 1.43x`, `zunion 1.07x`).

Investigated `SortedSet::lex_count`:
- The `0.24x` varying-score cell hits the O(n) `iter_asc().filter(lex_in_range)`
  fallback because the O(log n) rank-difference fast path requires
  `first.score == last.score`. ZLEXCOUNT on a varying-score zset is
  *unspecified* in Redis (it assumes one shared score), so this cell is not a
  realistic workload. fr and Redis still agree on the count for `- +` (both =
  cardinality); only the timing differs.
- The realistic equal-score path DOES take the warm-treap fast path (correctness
  verified: 0 diffs vs Redis across `- +`, `[lo [hi`, `(lo (hi`, half-open
  bounds). It allocates `ScoreMember::actual(s, x.to_vec())` (a `Vec` + an
  `Arc<[u8]>`) per `rank_of`, ×2, plus 2 `get_score` dict probes.

Decision: **NOT pursued.** Back-of-envelope: the store-side allocations +
2 treap descents total well under ~1µs, but the measured cost is ~3.8µs/call
(`0.38ms / 100` pipelined ops). The gap is therefore dominated by per-command
DISPATCH overhead (RESP parse + cold-command machinery in fr-runtime/fr-command),
not the `lex_count` body — an allocation-free borrowed `rank_of` was estimated to
land near `~0.86x`, still sub-parity, so it fails the "A/B must cross >1.0x"
bar. Real lever would be a ZLEXCOUNT dispatch borrow fast-path (cold-command
vein, fr-runtime), which is a separate, largely-exhausted domain. Absolute cost
is sub-5µs/call on a rarely-hot command → low Impact×Confidence/Effort. No source
hunk written. Score for this lever: **0 win / 0 loss / 1 declined-pre-build**.

## GEODIST float formatting — DECLINED on byte-exactness risk (BlackThrush 2026-06-20)

Re-profiling the post-cmdname binary (mix incl GEODIST) showed
`flt2dec::strategy::dragon::format_exact` at ~4% — `geo_distance_reply`'s
`format!("{normalized:.4}")` (fr-command). Rust's `{:.4}` runs the dragon
correctly-rounded fixed-precision algorithm; ~28% of GEODIST's per-call cost is
this formatter.

A faster integer-scaling path (`(d * 10000.0).round() as i64` then manual digit
emit, reusing the e4fu8 branchless decimal-length) would skip dragon entirely —
BUT `f64::round` is half-AWAY-from-zero while Rust `{:.4}` and Redis `%.4f` both
round half-to-EVEN, so the scaling path would byte-DIVERGE from vendored Redis on
exact `.00005` boundaries. GEODIST output must stay byte-exact (it is today), and
replicating `%.4f` round-half-to-even by hand is ~what dragon already does.
GEODIST is also one cheap, rarely-hot command (the 4% is an artifact of the
1/7-geodist micro-mix; geodist absolute cost is <0.5 ms/100 ops). Low
Impact×Confidence/Effort + correctness risk → NOT pursued. The eager-per-command
waste vein in `execute_frame_internal` is exhausted after clock-chaining (genclock,
-85M instr) and lazy command_name (-168M instr); residual dispatch hot functions
(command_table_index, classify_command, foldhash command-name hash,
parse_command_args_borrowed_into, dispatch_with_client_context) are necessary
per-command work, not removable waste. Score: **0 win / 0 loss / 1 declined**.
