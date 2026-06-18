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
- (add here as found) — prefer clean crates (fr-protocol, fr-persist non-LZF) not under a
  peer's active reservation; bench A/B in release before claiming a win.
