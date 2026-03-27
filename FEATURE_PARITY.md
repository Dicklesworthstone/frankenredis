# FEATURE_PARITY

Non-negotiable policy:
- This matrix tracks progress toward absolute, total drop-in parity.
- No row may be permanently excluded; sequencing deferrals must convert to closure work.

## Status Legend

- not_started
- in_progress
- parity_green
- parity_gap

## Parity Matrix

| Feature Family | Status | Notes |
|---|---|---|
| RESP protocol and command dispatch | in_progress | parser + 227 commands: strings (GETEX, SUBSTR, LCS, SET with EX/PX/EXAT/PXAT/KEEPTTL/NX/XX/GET), keys, hash, list (extended + LMPOP, LPOP/RPOP with COUNT), set (SMISMEMBER, SINTERCARD, SRANDMEMBER with COUNT, SPOP with COUNT), sorted set (ZUNIONSTORE, ZINTERSTORE, ZRANGESTORE, ZMPOP, ZDIFF, ZDIFFSTORE, ZINTER, ZUNION, ZINTERCARD, ZRANGE with BYSCORE/BYLEX/REV/LIMIT/WITHSCORES, ZRANGEBYSCORE/ZREVRANGEBYSCORE with WITHSCORES/LIMIT, ZRANGEBYLEX/ZREVRANGEBYLEX with LIMIT, ZPOPMIN/ZPOPMAX with COUNT), HyperLogLog including PFDEBUG (GETREG/DECODE/ENCODING/TODENSE) and PFSELFTEST, bitmap (BITOP, BITFIELD with full bit manipulation), SORT/SORT_RO (BY/GET/LIMIT/ALPHA/STORE), MULTI/EXEC/DISCARD/WATCH/UNWATCH transactions, SCAN family, server/connection commands (MEMORY, SLOWLOG with real timing/configurable threshold, SAVE/BGSAVE/BGREWRITEAOF/LASTSAVE, SWAPDB, OBJECT ENCODING/REFCOUNT/IDLETIME/FREQ/HELP, DEBUG, ROLE, SHUTDOWN, LATENCY, LOLWUT, WAITAOF, MODULE with HELP/LIST plus Redis-style LOAD/LOADEX/UNLOAD failure surfaces, COMMAND with COUNT/LIST/INFO/DOCS/GETKEYS (key extraction from COMMAND_TABLE metadata), READONLY/READWRITE), CLIENT (SETNAME/GETNAME/ID/LIST/INFO/KILL/PAUSE/UNPAUSE/TRACKING/CACHING/NO-EVICT/NO-TOUCH/SETINFO), CLUSTER (INFO/MYID/SLOTS/SHARDS/NODES/KEYSLOT/RESET), REPLICAOF/SLAVEOF, FUNCTION (LOAD/LIST/STATS/DUMP/RESTORE/FLUSH/DELETE/HELP), FCALL/FCALL_RO, Geo (GEOADD, GEOPOS, GEODIST, GEOHASH, GEORADIUS with STORE/STOREDIST, GEORADIUSBYMEMBER with STORE/STOREDIST, GEOSEARCH, GEOSEARCHSTORE), Streams (XADD/XLEN/XDEL/XTRIM/XREAD/XREADGROUP/XCLAIM/XAUTOCLAIM/XPENDING/XACK/XSETID/XINFO/XGROUP/XRANGE/XREVRANGE), COPY, DUMP/RESTORE with full type coverage, Pub/Sub with cross-client delivery (SUBSCRIBE/UNSUBSCRIBE/PSUBSCRIBE/PUNSUBSCRIBE/PUBLISH/PUBSUB, SSUBSCRIBE/SUNSUBSCRIBE/SPUBLISH), blocking infrastructure complete (BLPOP/BRPOP/BLMOVE/BLMPOP/BRPOPLPUSH/BZPOPMIN/BZPOPMAX/BZMPOP/XREAD BLOCK/XREADGROUP BLOCK), Lua scripting (EVAL/EVALSHA/EVAL_RO/EVALSHA_RO with full Lua 5.1 evaluator including variables, arithmetic, string concat, comparisons, logical ops, if/elseif/else, for/while/repeat loops, tables, function calls/definitions, redis.call/pcall, KEYS/ARGV, standard library), SCRIPT LOAD/EXISTS/FLUSH; exclusive score bounds via `(` prefix supported, SCAN TYPE filter, EXPIRETIME/PEXPIRETIME precision fix, MSETNX/COPY/OBJECT conformance fixes, ZRANDMEMBER with COUNT/WITHSCORES, HRANDFIELD with COUNT/WITHVALUES, ZADD NX/XX/GT/LT/CH options, LPOS with RANK/COUNT/MAXLEN, BITOP operation validation fix, ZRANGE REV rank-mode fix (now uses descending index like Redis), SORT_RO STORE rejection fix, OBJECT ENCODING intset detection for all-integer sets, Lua table.sort/table.insert/table.remove mutation write-back fix, Lua string pattern matching (string.match/gmatch/gsub/find with full pattern engine: character classes, quantifiers, anchors, captures, sets), table.sort with custom comparator, rawset mutation fix, string.format with full width/precision/flags support, xpcall error handling, math trig functions (sin/cos/tan/asin/acos/atan/atan2), math.log10/modf/frexp/ldexp, os.clock, redis.replicate_commands/set_repl/breakpoint/debug support with REPL_* constants, table.maxn sparse-key fix; COMMAND LIST FILTERBY MODULE/ACLCAT/PATTERN, INFO reports real stats (connected_clients, total_commands_processed, total_connections_received, run_id, process_id, tcp_port, used_memory, maxmemory_policy, dirty counter, expires count); keyspace notifications (CONFIG SET notify-keyspace-events with __keyspace@<db>__:<key> and __keyevent@<db>__:<event> pub/sub delivery), SELECT accepts DB 0-15, SWAPDB/MOVE accept valid indices, DEBUG SLEEP with real blocking; replication sync baseline now includes PSYNC/SYNC negotiation, FULLRESYNC snapshot apply, CONTINUE backlog replay, INFO replication offsets, and replica reconnect/stream handling; remaining work is broader legacy-oracle parity hardening and replication edge-case expansion rather than missing core sync plumbing |
| Core data types and keyspace | parity_green | All 7 data types (String, Hash, List, Set, Sorted Set, HyperLogLog, Geo) plus Streams fully implemented with WRONGTYPE enforcement, per-database key isolation, and comprehensive edge-case coverage. Stream consumer groups with PEL tracking, XCLAIM/XAUTOCLAIM ownership transfer, XPENDING IDLE filtering, XINFO CONSUMERS real metrics. OBJECT ENCODING with canonical int detection, intset for all-integer sets. 443+ unit tests, adversarial edge case tests, and dirty counter regression tests all pass. |
| TTL and eviction behavior | parity_green | Lazy expiry on all access paths, EXPIRE/PEXPIRE/EXPIREAT/PEXPIREAT with NX/XX/GT/LT options, TTL/PTTL/EXPIRETIME/PEXPIRETIME/PERSIST, active expire cycle with configurable budget, maxmemory eviction (allkeys-lru/volatile-lru/allkeys-random/volatile-random/noeviction) with configurable maxmemory-samples. Keyspace notifications for expired/evicted keys. 166 core_expiry conformance cases pass. |
| RDB/AOF persistence | in_progress | implemented/tested baseline includes AOF record framing, Store→AOF rewrite serialization for all supported data types plus TTL, atomic file I/O, `Runtime::load_aof` replay, SAVE/BGSAVE/BGREWRITEAOF wiring, and RDB snapshot encode/decode with CRC64 across supported value types; runtime and persist tests cover AOF round-trips and RDB read/write behavior, and `persist_replay` conformance is online and passing. Remaining work is deeper parity hardening around long-tail recovery ordering and broader legacy-oracle persistence semantics, not basic persistence bring-up |
| Replication baseline | in_progress | implemented/tested baseline includes backlog-window tracking, PSYNC decisioning, SYNC/FULLRESYNC replies, replica ACK/fsync accounting, WAIT/WAITAOF thresholds, full snapshot apply on replicas, CONTINUE backlog replay, and server-level replica reconnect/partial resync flow; remaining work is widening oracle coverage and hardening edge semantics, not initial protocol sync bring-up |
| ACL/config mode split | in_progress | implemented/tested baseline includes AUTH plus ACL lifecycle commands (`SETUSER`/`GETUSER`/`DELUSER`/`LIST`/`WHOAMI`/`CAT`/`GENPASS`/`LOG`) and broad live CONFIG GET/SET coverage in `fr-runtime`, including multi-pattern glob matching, memory/latency/replication/query-buffer knobs, appendonly/dir/dbfilename, and keyspace-notification configuration; `core_acl` and `core_config` conformance are online and passing. Remaining work is long-tail parameter and policy-surface expansion rather than initial ACL/CONFIG wiring |
| Differential conformance harness | in_progress | implemented/tested baseline includes general fixture execution plus dedicated protocol-negative and replay runners, FR-P2C packet-family suites (`FR-P2C-001` through `FR-P2C-009`), structured-log verification, and broad smoke coverage across core command families, packet journeys, protocol abuse, and persistence replay. Live Redis diff entry points also exist for side-by-side oracle checks when a local Redis instance is available. Remaining work is broader oracle depth and fixture expansion, not basic harness bring-up |
| Benchmark + optimization artifacts | in_progress | checked-in evidence currently lives under `artifacts/optimization/phase2c-gate` (baseline/after hyperfine summaries, stdout/sha256 captures, syscall profiles, and gate harness) plus `artifacts/optimization/ISOMORPHISM_PROOF_ROUND{1,2}.md` and `artifacts/phase2c/schema/topology_lock_v1.json`; remaining work is broader benchmark refresh/repopulation and packet-directory evidence expansion, not initial optimization-proof scaffolding |
| Full command/API surface closure | parity_green | All 241 Redis base commands have real implementations (zero stubs). Per-database key isolation implemented (encode_db_key namespace prefix, DB-scoped KEYS/DBSIZE/FLUSHDB/RANDOMKEY/SCAN/MOVE/COPY, SWAPDB). COMMAND DOCS, COMMAND GETKEYSANDFLAGS, PFDEBUG, PFSELFTEST, MONITOR (streaming), MIGRATE (DUMP/RESTORE over TCP), FAILOVER (standalone validation), MODULE (proper error handling), SENTINEL (non-sentinel mode), CONFIG HELP. CLIENT PAUSE with actual blocking and starvation fix. SHUTDOWN graceful exit with optional SAVE. Lua closures with upvalue capture. DUMP/RESTORE upgraded to CRC64 with consistent bounds checks. XPENDING IDLE option (Redis 6.2+). XINFO CONSUMERS with real pending/idle metrics. CONFIG SET/GET fully wired for maxclients, hz, busy-reply-threshold, lua-time-limit, maxmemory-samples, repl-backlog-size, repl-timeout, client-query-buffer-limit, proto-max-bulk-len, client-output-buffer-limit, appendonly, dir, dbfilename. NUM_DATABASES runtime-configurable via Vec. Per-client peer address tracking for CLIENT LIST. Stream RDB persistence. OBJECT ENCODING canonical int check. 435+ unit tests, 3700+ conformance cases, 39 smoke tests all pass. |

## Required Evidence Per Feature Family

1. Differential fixture report.
2. Edge-case/adversarial test results.
3. Benchmark delta (when performance-sensitive).
4. Documented compatibility exceptions only as temporary sequencing notes with blocking closure IDs.

## Current Evidence Pointers

- `crates/fr-conformance/fixtures/core_strings.json`
- `crates/fr-conformance/fixtures/core_errors.json`
- `crates/fr-conformance/fixtures/protocol_negative.json`
- `crates/fr-conformance/fixtures/core_hash.json`
- `crates/fr-conformance/fixtures/core_list.json`
- `crates/fr-conformance/fixtures/core_set.json`
- `crates/fr-conformance/fixtures/core_zset.json`
- `crates/fr-conformance/fixtures/core_generic.json`
- `crates/fr-conformance/fixtures/core_geo.json`
- `crates/fr-conformance/fixtures/core_stream.json`
- `crates/fr-conformance/fixtures/core_acl.json`
- `crates/fr-conformance/fixtures/core_hyperloglog.json`
- `crates/fr-conformance/fixtures/core_bitmap.json`
- `crates/fr-conformance/fixtures/core_transaction.json`
- `crates/fr-conformance/fixtures/core_connection.json`
- `crates/fr-conformance/fixtures/core_expiry.json`
- `crates/fr-conformance/fixtures/core_client.json`
- `crates/fr-conformance/fixtures/core_server.json`
- `crates/fr-conformance/fixtures/core_scripting.json`
- `crates/fr-conformance/fixtures/core_sort.json`
- `crates/fr-conformance/fixtures/core_scan.json`
- `crates/fr-conformance/fixtures/core_config.json`
- `crates/fr-conformance/fixtures/core_cluster.json`
- `crates/fr-conformance/fixtures/core_copy.json`
- `crates/fr-conformance/fixtures/core_function.json`
- `crates/fr-conformance/fixtures/core_wait.json`
- `crates/fr-conformance/fixtures/persist_replay.json`
- `artifacts/optimization/phase2c-gate/baseline_hyperfine.json`
- `artifacts/optimization/phase2c-gate/baseline_strace.txt`
- `artifacts/optimization/phase2c-gate/after_hyperfine_multi.json`
- `artifacts/optimization/phase2c-gate/after_multi_strace.txt`
- `artifacts/optimization/ISOMORPHISM_PROOF_ROUND1.md`
- `artifacts/optimization/ISOMORPHISM_PROOF_ROUND2.md`
- `artifacts/phase2c/schema/topology_lock_v1.json`
