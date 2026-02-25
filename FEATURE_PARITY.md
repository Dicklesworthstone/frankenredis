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
| RESP protocol and command dispatch | in_progress | parser + 140+ commands: strings (GETEX, SUBSTR, LCS), keys, hash, list (extended + LMPOP), set (SMISMEMBER, SINTERCARD), sorted set (ZUNIONSTORE, ZINTERSTORE, ZMPOP, ZDIFF, ZDIFFSTORE, ZINTER, ZUNION, ZINTERCARD), HyperLogLog, bitmap (BITOP, BITFIELD), MULTI/EXEC/DISCARD/WATCH/UNWATCH transactions, SCAN family, server/connection commands (MEMORY, SLOWLOG, SAVE/BGSAVE/BGREWRITEAOF/LASTSAVE, SWAPDB, OBJECT ENCODING, DEBUG, ROLE, SHUTDOWN, LATENCY), Geo (GEOADD, GEOPOS, GEODIST, GEOHASH, GEORADIUS, GEORADIUSBYMEMBER, GEOSEARCH, GEOSEARCHSTORE), Streams, Pub/Sub stubs (SUBSCRIBE/UNSUBSCRIBE/PSUBSCRIBE/PUNSUBSCRIBE/PUBLISH/PUBSUB), blocking ops stubs (BLPOP/BRPOP/BLMOVE/BLMPOP, BRPOPLPUSH), EVAL/EVALSHA/SCRIPT stubs, cluster stubs; missing: Lua scripting engine, full blocking semantics, full Pub/Sub message delivery |
| Core data types and keyspace | in_progress | String, Hash, List, Set, Sorted Set, HyperLogLog, and Geo data types implemented with full WRONGTYPE enforcement; Streams starter slice (`XADD`, `XLEN`, `XDEL`, `XTRIM`, `XREAD`, `XINFO STREAM/GROUPS/CONSUMERS`, `XGROUP CREATE`, `XGROUP DESTROY`, `XGROUP SETID`, `XGROUP CREATECONSUMER`, `XGROUP DELCONSUMER`, `XRANGE`, `XREVRANGE`) implemented, broader stream surface pending |
| TTL and eviction behavior | in_progress | lazy expiry and `PTTL` semantics scaffolded (`-2/-1/remaining`) |
| RDB/AOF persistence | in_progress | AOF record frame contract scaffolded; full replay fidelity pending |
| Replication baseline | in_progress | state/offset progression scaffolded; protocol sync semantics pending |
| ACL/config mode split | in_progress | ACL command subsystem implemented (AUTH, ACL SETUSER/GETUSER/DELUSER/LIST/WHOAMI); CONFIG GET/SET implemented in fr-runtime; full parameter surface and ACL CAT/GENPASS/LOG pending |
| Differential conformance harness | in_progress | fixture runner online for `core_strings`, `core_errors`, `core_hash`, `core_list`, `core_set`, `core_zset`, `core_geo`, `core_stream`, `protocol_negative`, and `persist_replay` suites |
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
- `crates/fr-conformance/fixtures/core_geo.json`
- `crates/fr-conformance/fixtures/core_stream.json`
- `crates/fr-conformance/fixtures/persist_replay.json`
- `baselines/round1_conformance_baseline.json`
- `baselines/round1_conformance_strace.txt`
- `baselines/round2_protocol_negative_baseline.json`
- `baselines/round2_protocol_negative_strace.txt`
- `golden_checksums.txt`
