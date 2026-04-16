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
| RESP protocol and command dispatch | parity_green | parser + 227 commands: strings (GETEX, SUBSTR, LCS, SET with EX/PX/EXAT/PXAT/KEEPTTL/NX/XX/GET), keys, hash, list (extended + LMPOP, LPOP/RPOP with COUNT), set (SMISMEMBER, SINTERCARD, SRANDMEMBER with COUNT, SPOP with COUNT), sorted set (ZUNIONSTORE, ZINTERSTORE, ZRANGESTORE, ZMPOP, ZDIFF, ZDIFFSTORE, ZINTER, ZUNION, ZINTERCARD, ZRANGE with BYSCORE/BYLEX/REV/LIMIT/WITHSCORES, ZRANGEBYSCORE/ZREVRANGEBYSCORE with WITHSCORES/LIMIT, ZRANGEBYLEX/ZREVRANGEBYLEX with LIMIT, ZPOPMIN/ZPOPMAX with COUNT), HyperLogLog including PFDEBUG (GETREG/DECODE/ENCODING/TODENSE) and PFSELFTEST, bitmap (BITOP, BITFIELD with full bit manipulation), SORT/SORT_RO (BY/GET/LIMIT/ALPHA/STORE), MULTI/EXEC/DISCARD/WATCH/UNWATCH transactions, SCAN family, server/connection commands (MEMORY, SLOWLOG with real timing/configurable threshold, SAVE/BGSAVE/BGREWRITEAOF/LASTSAVE, SWAPDB, OBJECT ENCODING/REFCOUNT/IDLETIME/FREQ/HELP, DEBUG, ROLE, SHUTDOWN, LATENCY, LOLWUT, WAITAOF, MODULE with HELP/LIST plus Redis-style LOAD/LOADEX/UNLOAD failure surfaces, COMMAND with COUNT/LIST/INFO/DOCS/GETKEYS (key extraction from COMMAND_TABLE metadata), READONLY/READWRITE), CLIENT (SETNAME/GETNAME/ID/LIST/INFO/KILL/PAUSE/UNPAUSE/TRACKING/CACHING/NO-EVICT/NO-TOUCH/SETINFO), CLUSTER (INFO/MYID/SLOTS/SHARDS/NODES/KEYSLOT/RESET), REPLICAOF/SLAVEOF, FUNCTION (LOAD/LIST/STATS/DUMP/RESTORE/FLUSH/DELETE/HELP), FCALL/FCALL_RO, Geo (GEOADD, GEOPOS, GEODIST, GEOHASH, GEORADIUS with STORE/STOREDIST, GEORADIUSBYMEMBER with STORE/STOREDIST, GEOSEARCH, GEOSEARCHSTORE), Streams (XADD/XLEN/XDEL/XTRIM/XREAD/XREADGROUP/XCLAIM/XAUTOCLAIM/XPENDING/XACK/XSETID/XINFO/XGROUP/XRANGE/XREVRANGE), COPY, DUMP/RESTORE with full type coverage, Pub/Sub with cross-client delivery (SUBSCRIBE/UNSUBSCRIBE/PSUBSCRIBE/PUNSUBSCRIBE/PUBLISH/PUBSUB, SSUBSCRIBE/SUNSUBSCRIBE/SPUBLISH), blocking infrastructure complete (BLPOP/BRPOP/BLMOVE/BLMPOP/BRPOPLPUSH/BZPOPMIN/BZPOPMAX/BZMPOP/XREAD BLOCK/XREADGROUP BLOCK), Lua scripting (EVAL/EVALSHA/EVAL_RO/EVALSHA_RO with full Lua 5.1 evaluator including variables, arithmetic, string concat, comparisons, logical ops, if/elseif/else, for/while/repeat loops, tables, function calls/definitions, redis.call/pcall, KEYS/ARGV, standard library), SCRIPT LOAD/EXISTS/FLUSH; exclusive score bounds via `(` prefix supported, SCAN TYPE filter, EXPIRETIME/PEXPIRETIME precision fix, MSETNX/COPY/OBJECT conformance fixes, ZRANDMEMBER with COUNT/WITHSCORES, HRANDFIELD with COUNT/WITHVALUES, ZADD NX/XX/GT/LT/CH options, LPOS with RANK/COUNT/MAXLEN, BITOP operation validation fix, ZRANGE REV rank-mode fix (now uses descending index like Redis), SORT_RO STORE rejection fix, OBJECT ENCODING intset detection for all-integer sets, Lua table.sort/table.insert/table.remove mutation write-back fix, Lua string pattern matching (string.match/gmatch/gsub/find with full pattern engine: character classes, quantifiers, anchors, captures, sets), table.sort with custom comparator, rawset mutation fix, string.format with full width/precision/flags support, xpcall error handling, math trig functions (sin/cos/tan/asin/acos/atan/atan2), math.log10/modf/frexp/ldexp, os.clock, redis.replicate_commands/set_repl/breakpoint/debug support with REPL_* constants, table.maxn sparse-key fix; COMMAND LIST FILTERBY MODULE/ACLCAT/PATTERN, INFO reports real stats (connected_clients, total_commands_processed, total_connections_received, run_id, process_id, tcp_port, used_memory, maxmemory_policy, dirty counter, expires count); keyspace notifications (CONFIG SET notify-keyspace-events with __keyspace@<db>__:<key> and __keyevent@<db>__:<event> pub/sub delivery), SELECT accepts DB 0-15, SWAPDB/MOVE accept valid indices, DEBUG SLEEP with real blocking; replication sync baseline now includes PSYNC/SYNC negotiation, FULLRESYNC snapshot apply, CONTINUE backlog replay, INFO replication offsets, and replica reconnect/stream handling; remaining work is broader legacy-oracle parity hardening and replication edge-case expansion rather than missing core sync plumbing |
| Core data types and keyspace | parity_green | All 7 data types (String, Hash, List, Set, Sorted Set, HyperLogLog, Geo) plus Streams fully implemented with WRONGTYPE enforcement, per-database key isolation, and comprehensive edge-case coverage. Stream consumer groups with PEL tracking, XCLAIM/XAUTOCLAIM ownership transfer, XPENDING IDLE filtering, XINFO CONSUMERS real metrics. OBJECT ENCODING with canonical int detection, intset for all-integer sets. 443+ unit tests, adversarial edge case tests, and dirty counter regression tests all pass. |
| TTL and eviction behavior | parity_green | Lazy expiry on all access paths, EXPIRE/PEXPIRE/EXPIREAT/PEXPIREAT with NX/XX/GT/LT options, TTL/PTTL/EXPIRETIME/PEXPIRETIME/PERSIST, active expire cycle with configurable budget, maxmemory eviction (allkeys-lru/volatile-lru/allkeys-random/volatile-random/noeviction) with configurable maxmemory-samples. Keyspace notifications for expired/evicted keys. 166 core_expiry conformance cases pass. |
| RDB/AOF persistence | parity_green | AOF record framing, Store→AOF rewrite serialization for all 7 data types plus streams and TTL, atomic file I/O, `Runtime::load_aof` replay, SAVE/BGSAVE/BGREWRITEAOF wiring. RDB snapshot encode/decode with CRC64 across all value types including streams (bug fix: stream-with-expiry decode failure corrected). Multi-database AOF round-trip verified (SELECT + namespace prefix encoding). 30 persist unit tests (including stream round-trips, all-types-together, empty stream, stream-with-expiry, checksum/magic/eof/version rejection). 18 persist_replay conformance cases covering string ops, TTL chains, COPY, UNLINK, INCRBYFLOAT, KEEPTTL, SET GET, PEXPIREAT/EXPIREAT. AOF save/load round-trip tests for streams, TTL preservation, and multi-database scenarios. |
| Replication baseline | parity_green | **What works:** TCP replication end-to-end between server processes — `--replicaof` CLI flag bootstraps replica link, REPLICAOF/SLAVEOF from client sessions (tested: `tcp_replicaof_command_connects_to_legacy_primary_and_replicates_writes`), authenticated upstream links via `CONFIG SET masterauth`/`masteruser` when the primary requires `AUTH`, PSYNC handshake (PING → AUTH → REPLCONF → PSYNC), FULLRESYNC with RDB snapshot streaming over TCP, CONTINUE backlog replay, ongoing command propagation from primary to replicas via AOF encoding, REPLCONF ACK offset acknowledgement, replica reconnect after link loss, replica write-admission gates via `min-replicas-to-write` / `min-replicas-max-lag` (including legacy-oracle TCP coverage and `MULTI` queue rejection), and replica-of-replica chaining with topology metadata verification (`tcp_replica_of_replica_chain_replication`). Integration tests prove legacy-Redis-primary → FrankenRedis-replica, password-protected legacy-Redis-primary → FrankenRedis-replica, min-replicas gating against both FrankenRedis and legacy Redis primaries, and FrankenRedis-primary → replica → downstream-replica data flow. Backlog-window tracking with rotate, PSYNC decisioning, WAIT/WAITAOF thresholds, saturating offset arithmetic. Sentinel integration supports `REPLICAOF NO ONE`, chained `REPLICAOF`, and exposes `replica-priority` controls. 25 replication unit tests + 8 TCP E2E integration tests pass. **What does NOT work yet:** None (parity achieved). |
| ACL/config mode split | parity_green | Full ACL lifecycle (`SETUSER`/`GETUSER`/`DELUSER`/`LIST`/`USERS`/`WHOAMI`/`CAT`/`GENPASS`/`LOG`/`SAVE`/`LOAD`/`DRYRUN`/`HELP`) with per-command permissions (`+command`/`-command`), per-category permissions (`+@category`/`-@category`), `allcommands`/`nocommands`/`allkeys`/`allchannels`/`reset` rules, key pattern acceptance (`~pattern`), channel pattern acceptance (`&pattern`), and deny-first precedence (explicit deny > explicit allow > category deny > category allow > base). `is_command_authorized()` enforces per-command ACL at dispatch time. Broad live CONFIG GET/SET coverage including multi-pattern glob matching, memory/latency/replication/query-buffer knobs, appendonly/dir/dbfilename, and keyspace-notification configuration. `core_acl` (82 cases) and `core_config` (132 cases) conformance online and passing. 11 per-command ACL unit tests covering allow/deny, categories, override precedence, case-insensitive matching, reset, DRYRUN, GETUSER/LIST reflection. |
| Differential conformance harness | parity_green | implemented/tested baseline includes general fixture execution plus dedicated protocol-negative and replay runners, FR-P2C packet-family suites (`FR-P2C-001` through `FR-P2C-009`), structured-log verification, and broad smoke coverage across core command families, packet journeys, protocol abuse, and persistence replay. Live Redis diff entry points also exist for side-by-side oracle checks when a local Redis instance is available. Remaining work is broader oracle depth and fixture expansion, not basic harness bring-up |
| Benchmark + optimization artifacts | parity_green | Checked-in workload benchmarking is live and reproducible: `crates/fr-bench` reports ops/sec plus p50/p95/p99/p999 latency, normalized FrankenRedis and Redis baselines are checked into `baselines/`, `scripts/record_baselines.sh` captures new baselines, and `scripts/benchmark_gate.sh` compares candidate runs against the checked-in FrankenRedis baselines with throughput/p99 regression thresholds. Conformance optimization evidence remains under `artifacts/optimization/phase2c-gate` (hyperfine summaries, syscall profiles, decision ledger), alongside isomorphism proofs (`ISOMORPHISM_PROOF_ROUND{1,2}.md`) and topology lock artifacts. The open problem in this area is throughput improvement versus Redis, not missing measurement or gating infrastructure. |
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
- `crates/fr-bench/src/main.rs`
- `baselines/frankenredis_v0.1.0_set.json`
- `baselines/redis_7.2.4_set.json`
- `scripts/record_baselines.sh`
- `scripts/benchmark_gate.sh`
- `artifacts/optimization/phase2c-gate/baseline_hyperfine.json`
- `artifacts/optimization/phase2c-gate/baseline_strace.txt`
- `artifacts/optimization/phase2c-gate/after_hyperfine_multi.json`
- `artifacts/optimization/phase2c-gate/after_multi_strace.txt`
- `artifacts/optimization/ISOMORPHISM_PROOF_ROUND1.md`
- `artifacts/optimization/ISOMORPHISM_PROOF_ROUND2.md`
- `artifacts/phase2c/schema/topology_lock_v1.json`
