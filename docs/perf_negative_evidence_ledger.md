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
| Hand-rolled large-buffer reuse / malloc-avoidance | 0.77–0.93x (REGRESSION) | mimalloc (fr default) already recycles large buffers; hand reuse fights it. A/B before trusting any malloc-avoidance lever. |
| ChunkedList → VecDeque / decode-path rewrite for list RESTORE | 0.53x (SLOWER) | per-element alloc is the cost; VecDeque didn't help. Real lever = packed-listpack-node ChunkedList (99fwc), not container swap. |
| SWAR/SIMD on memory-bound byte loops (max/copy/fill, HLL register-max) | ~1.0x (0.94x for HLL) | only COMPUTE-bound loops win (popcount/CRC/bitwise = 4–13x). Check compute-vs-memory first. Clean-crate compute kernels already done. |
| used_memory via counting-allocator | ~7% throughput hit + wrong target | estimate_memory_usage_bytes MODELS redis; counting-alloc measures fr's actual RAM (a different number). RSS lags frees. Don't "fix" the model with real accounting. |
| zadd 8% pipelined gap | WONTFIX (x1zbp) | distributed across dispatch, no single hot spot. |

## Real residual gaps (structural; mind ownership before touching)
| Gap | Ratio | Owner / bead | Note |
|---|---|---|---|
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
  `memchr::memchr` — CODED (reasoned; batch benchmark pending). Guard covers
  CR-not-LF scanning plus exact `MAX_LINE_LENGTH` `Incomplete`/`LineTooLong`
  boundaries. Retry condition if rejected: only revisit with a fresh parser
  self-time row or a benchmark that isolates line scanning from runtime/server
  packet-parser work.
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
  machinery — CODED (reasoned; batch benchmark pending). Scope is store-side byte
  materialization for integer GET-like paths and `SetValue::Int` iteration /
  promotion / removal; RESP serializer, runtime, and server code are unchanged.
  Guard pins zero, sign edges, and i64 min/max against the old `to_string`
  reference for `Value::Integer` and intset member materialization. Retry
  condition if rejected: do not retry generic i64 formatting cleanup unless a
  fresh profile names integer materialization or intset member formatting.
- frankenredis-gu5nf.32 / cod-a: `fr-store` `SetValue::retain` now feeds intset
  predicates stack-borrowed decimal bytes through `with_integer_decimal_bytes`
  instead of allocating a `Vec<u8>` per retained member — CODED (reasoned; batch
  benchmark pending). Scope is the mixed-encoding set-algebra fallback path where
  an intset is filtered by a byte predicate; direct intset/intset merge kernels
  and reply encoding are unchanged. Guard pins stack-borrowed bytes against the
  old `to_string` reference and checks retain membership/order for i64 min/max,
  negative, zero, and positive values. Retry condition if rejected: do not retry
  intset predicate byte formatting unless a fresh profile names `SetValue::retain`
  or mixed intset/generic set-algebra allocation cost.
- frankenredis-n2u1g / cod-b: zset score direct encoder for borrowed `ZSCORE`
  and `ZMSCORE` network fast paths — CODED (reasoned; batch benchmark pending).
  `fr-protocol::encode_redis_double` writes Redis d2string bytes directly into
  RESP3 Double / RESP2 bulk-string frames, and fr-runtime/fr-server now use it
  for score-read fast paths instead of allocating a `String`/score `RespFrame`.
  Guard compares raw wire bytes against generic dispatch for RESP2, RESP3, nil,
  and WRONGTYPE paths. Retry condition if rejected: do not add a wide
  `RespFrame` score variant or option-bearing `ZRANGE WITHSCORES` direct path
  unless a release profile names score formatting/allocation in a zset
  WITHSCORES workload.
- frankenredis-n2u1g / cod-b: direct encoder for canonical rank-form
  `ZRANGE key start stop WITHSCORES` — CODED (reasoned after the dedicated
  `fr-bench --workload zrange-withscores` harness landed; batch profile and
  criterion vs Redis pending). RESP2 emits the flat upstream shape
  `member,score,...`; RESP3 emits `[member,score]` pair subarrays and writes
  score doubles through the existing direct Redis d2string encoder. Generic
  `REV`/`BYSCORE`/`BYLEX`/`LIMIT` option shapes still fall through to canonical
  dispatch. Guard compares raw wire bytes against generic dispatch for RESP2,
  RESP3, missing-key empty arrays, WRONGTYPE errors, and bad-integer fallback.
  Retry condition if rejected: do not expand to `ZREVRANGE`,
  `ZRANGEBYSCORE WITHSCORES`, or `ZRANGE ... LIMIT` direct encoders unless the
  focused zrange-withscores bench or a release profile isolates those exact
  option shapes as score-format/allocation bottlenecks.
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
  now consumes owned decoded listpack entries with `into_bytes()` instead of
  cloning string payloads through `to_bytes()` and dropping the original —
  CODED (reasoned; batch benchmark pending). PLAIN quicklist2 nodes still move
  the raw node blob directly; integer listpack entries still render through the
  same canonical decimal helper. Guard is the quicklist2 packed-list decode/DUMP
  RDB suite plus crate-scoped check. Retry condition if rejected: do not retry
  quicklist2 owned-entry move cleanup unless a fresh DEBUG RELOAD / RESTORE
  profile names packed quicklist2 listpack decode allocation.
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
