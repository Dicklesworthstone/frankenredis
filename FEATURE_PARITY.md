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
| RESP protocol and command dispatch | in_progress | parser + 227 commands: strings (GETEX, SUBSTR, LCS, SET with EX/PX/EXAT/PXAT/KEEPTTL/NX/XX/GET), keys, hash, list (extended + LMPOP, LPOP/RPOP with COUNT), set (SMISMEMBER, SINTERCARD, SRANDMEMBER with COUNT, SPOP with COUNT), sorted set (ZUNIONSTORE, ZINTERSTORE, ZRANGESTORE, ZMPOP, ZDIFF, ZDIFFSTORE, ZINTER, ZUNION, ZINTERCARD, ZRANGE with BYSCORE/BYLEX/REV/LIMIT/WITHSCORES, ZRANGEBYSCORE/ZREVRANGEBYSCORE with WITHSCORES/LIMIT, ZRANGEBYLEX/ZREVRANGEBYLEX with LIMIT, ZPOPMIN/ZPOPMAX with COUNT), HyperLogLog, bitmap (BITOP, BITFIELD with full bit manipulation), SORT/SORT_RO (BY/GET/LIMIT/ALPHA/STORE), MULTI/EXEC/DISCARD/WATCH/UNWATCH transactions, SCAN family, server/connection commands (MEMORY, SLOWLOG, SAVE/BGSAVE/BGREWRITEAOF/LASTSAVE, SWAPDB, OBJECT ENCODING/REFCOUNT/IDLETIME/FREQ/HELP, DEBUG, ROLE, SHUTDOWN, LATENCY, LOLWUT, WAITAOF, COMMAND with COUNT/LIST/INFO/DOCS/GETKEYS, READONLY/READWRITE), CLIENT (SETNAME/GETNAME/ID/LIST/INFO/KILL/PAUSE/UNPAUSE/TRACKING/CACHING/NO-EVICT/NO-TOUCH), CLUSTER (INFO/MYID/SLOTS/SHARDS/NODES/KEYSLOT/RESET), REPLICAOF/SLAVEOF, FUNCTION (LOAD/LIST/STATS/DUMP/RESTORE/FLUSH/DELETE/HELP), FCALL/FCALL_RO, Geo (GEOADD, GEOPOS, GEODIST, GEOHASH, GEORADIUS, GEORADIUSBYMEMBER, GEOSEARCH, GEOSEARCHSTORE), Streams (XADD/XLEN/XDEL/XTRIM/XREAD/XREADGROUP/XCLAIM/XAUTOCLAIM/XPENDING/XACK/XSETID/XINFO/XGROUP/XRANGE/XREVRANGE), COPY, DUMP/RESTORE with full type coverage, Pub/Sub stubs (SUBSCRIBE/UNSUBSCRIBE/PSUBSCRIBE/PUNSUBSCRIBE/PUBLISH/PUBSUB, SSUBSCRIBE/SUNSUBSCRIBE/SPUBLISH), blocking ops stubs (BLPOP/BRPOP/BLMOVE/BLMPOP, BRPOPLPUSH), Lua scripting (EVAL/EVALSHA/EVAL_RO/EVALSHA_RO with full Lua 5.1 evaluator including variables, arithmetic, string concat, comparisons, logical ops, if/elseif/else, for/while/repeat loops, tables, function calls/definitions, redis.call/pcall, KEYS/ARGV, standard library), SCRIPT LOAD/EXISTS/FLUSH; exclusive score bounds via `(` prefix supported, SCAN TYPE filter, EXPIRETIME/PEXPIRETIME precision fix, MSETNX/COPY/OBJECT conformance fixes, ZRANDMEMBER with COUNT/WITHSCORES, HRANDFIELD with COUNT/WITHVALUES, ZADD NX/XX/GT/LT/CH options, LPOS with RANK/COUNT/MAXLEN; missing: full blocking semantics, full Pub/Sub message delivery |
| Core data types and keyspace | in_progress | String, Hash, List, Set, Sorted Set, HyperLogLog, and Geo data types implemented with full WRONGTYPE enforcement; Streams fully implemented (`XADD`, `XLEN`, `XDEL`, `XTRIM`, `XREAD`, `XREADGROUP`, `XCLAIM`, `XAUTOCLAIM`, `XPENDING`, `XACK`, `XSETID`, `XINFO STREAM/GROUPS/CONSUMERS`, `XGROUP CREATE/DESTROY/SETID/CREATECONSUMER/DELCONSUMER`, `XRANGE`, `XREVRANGE`) |
| TTL and eviction behavior | in_progress | lazy expiry and `PTTL` semantics scaffolded (`-2/-1/remaining`) |
| RDB/AOF persistence | in_progress | AOF record frame contract scaffolded; full replay fidelity pending |
| Replication baseline | in_progress | state/offset progression scaffolded; protocol sync semantics pending |
| ACL/config mode split | in_progress | ACL command subsystem implemented (AUTH, ACL SETUSER/GETUSER/DELUSER/LIST/WHOAMI); CONFIG GET/SET implemented in fr-runtime; full parameter surface and ACL CAT/GENPASS/LOG pending |
| Differential conformance harness | in_progress | fixture runner online for `core_strings`, `core_errors`, `core_hash`, `core_list`, `core_set`, `core_zset`, `core_geo`, `core_stream`, `core_generic`, `core_acl`, `core_hyperloglog`, `core_bitmap`, `core_transaction`, `core_connection`, `core_expiry`, `core_client`, `core_server`, `core_scripting`, `protocol_negative`, and `persist_replay` suites (780+ core conformance cases) |
| Benchmark + optimization artifacts | in_progress | round1 + round2 baseline JSON, syscall profile, and expanded golden checksum artifacts added |
| Full command/API surface closure | not_started | program-level closure row; all deferred families must roll up here before release sign-off |

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
- `crates/fr-conformance/fixtures/persist_replay.json`
- `baselines/round1_conformance_baseline.json`
- `baselines/round1_conformance_strace.txt`
- `baselines/round2_protocol_negative_baseline.json`
- `baselines/round2_protocol_negative_strace.txt`
- `golden_checksums.txt`
