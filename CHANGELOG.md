# Changelog

All notable changes to FrankenRedis are documented in this file.

FrankenRedis is a clean-room Rust reimplementation of Redis targeting full drop-in replacement parity
with deterministic latency, mathematical rigor, and memory safety. This project has no tagged releases
or GitHub Releases; the changelog is organized by date-bounded development phases derived from the
linear commit history on `main`. Workspace version: **0.1.0**.

Repository: <https://github.com/Dicklesworthstone/frankenredis>

---

## [Unreleased] -- development on `main` (as of 2026-05-16)

2354 commits across 78 active development days. 13-crate Cargo workspace (`fr-protocol`,
`fr-command`, `fr-store`, `fr-expire`, `fr-persist`, `fr-repl`, `fr-config`, `fr-conformance`,
`fr-runtime`, `fr-eventloop`, `fr-server`, `fr-bench`, `fr-sentinel`). 241 Redis base commands
with zero stubs. 4,975 conformance fixture cases across 43 fixture families. Throughput within
single-command parity range of Redis 7.2.4 after the April optimization sweep. No tags, no releases.

---

## Phase 11 -- Comprehensive Parity Hardening via Differential Probe Sweeps (2026-05-01 .. 2026-05-16)

A sustained "differential probe" methodology dominates this window: adversarial command sequences
are run against both FrankenRedis and a vendored Redis 7.2.4 oracle, every wire-level divergence
is filed as a beads issue, and each sweep closes another batch of tail parity quirks
(commit messages tagged `probe sweep #N`). The Lua scripting layer reaches metamethod completeness,
the DEBUG subsystem fills in, and a long stream of small-but-load-bearing wording, encoding, and
arity fixes lands.

### Lua metamethod completion

The metamethod epic closes — `__index`, `__newindex`, `__call`, `__concat`, `__add`/`__sub`/`__mul`/
`__div`/`__mod`/`__pow`/`__unm`, `__eq`, `__lt`, `__le`, `__tostring` all match vendored Lua 5.1
semantics including metatable protection and dispatch ordering. Function values become legal table
keys; `gmatch` iterators are callable outside of `for-in`.

- Closes the metamethods epic with `__tostring`
  ([e1c3196](https://github.com/Dicklesworthstone/frankenredis/commit/e1c31960acffc974b85712444b66a98d7ad9be7c))
- Comparison metamethods `__eq` / `__lt` / `__le`
  ([bf7914e](https://github.com/Dicklesworthstone/frankenredis/commit/bf7914ea564bf19c3114aefd7ab3d48b6a802465))
- Arithmetic metamethods `__add` / `__sub` / `__mul` / `__div` / `__mod` / `__pow` / `__unm`
  ([1e743a3](https://github.com/Dicklesworthstone/frankenredis/commit/1e743a390c77a28931d47acc33da25d23ca57e23))
- `__concat` metamethod for the `..` operator
  ([5d39297](https://github.com/Dicklesworthstone/frankenredis/commit/5d39297c07f6ac2ac0c672455a211a58a1529753))
- `__newindex` metamethod for table assignment
  ([1812792](https://github.com/Dicklesworthstone/frankenredis/commit/18127921d7be432f0d21e51fdb114c12e4bfe2c3))
- `__call` metamethod for callable tables
  ([4c232e1](https://github.com/Dicklesworthstone/frankenredis/commit/4c232e1c29476d4512bc27c137451ff33ce1e0aa))
- `__index` metamethod as function
  ([ec11d3b](https://github.com/Dicklesworthstone/frankenredis/commit/ec11d3bcc0de3fd109c14f8965d5a3213bf7a2e9))
- `gmatch` iterator callable outside `for-in`
  ([fc0f6e6](https://github.com/Dicklesworthstone/frankenredis/commit/fc0f6e6d272815b015a794fb557a64607dad85c7))
- Support function values as table keys
  ([145028c](https://github.com/Dicklesworthstone/frankenredis/commit/145028cdbf92ea0718b7a29e5ec7ca4ee7d46a1a))

### Lua sandbox surface and standard-library breadth

The Lua sandbox now exposes the full vendored Redis 7.2.4 surface: `_VERSION`, `rawequal`, `gcinfo`,
`collectgarbage`, `loadstring`/`load`, the LuaJIT-compatible `bit` library, additional trig/`math`
functions, NaN-sign-preserving `tostring`, and the full pattern matcher (`%b` balanced match,
`%f` frontier, `%1`-`%9` back-references). `cjson.encode`/`decode` argument validation and
formatting match upstream `%.14g`.

- Expose Lua `_G` sandbox surface
  ([cb30629](https://github.com/Dicklesworthstone/frankenredis/commit/cb306294f7e92eafe9b3b1afc1ad4adf61b0a8b1))
- `loadstring` / `load` registered in sandbox
  ([f6fb83e](https://github.com/Dicklesworthstone/frankenredis/commit/f6fb83eb2257e76e44d0b8d3a99c93af83e2afb6))
- Sandbox exposes `_VERSION`, `rawequal`, `gcinfo`, `collectgarbage`
  ([dd71244](https://github.com/Dicklesworthstone/frankenredis/commit/dd71244cb53af3afa6f5a3f47b85a3f8e75db5b6))
- LuaJIT-compatible `bit` library
  ([59ffe5a](https://github.com/Dicklesworthstone/frankenredis/commit/59ffe5a64c44c8de13f8ddba4bb9f8c0e5c1f81f))
- `math.deg`/`rad`/`sinh`/`cosh`/`tanh` + NaN-sign `tostring`
  ([18d9d14](https://github.com/Dicklesworthstone/frankenredis/commit/18d9d14ed3df744f23a3e9c8f88d7b3a8e75db5b))
- `cjson.encode`/`decode` argument validation and pcall shape
  ([48d0729](https://github.com/Dicklesworthstone/frankenredis/commit/48d0729a89c0ec0f7c5396c3e5d28cce9b8d4c52))
- `cjson.encode` matches upstream `%.14g` formatting and escapes `/`
  ([fd7153d](https://github.com/Dicklesworthstone/frankenredis/commit/fd7153d8e44c6e5b94db5b8bb3df5b9c1b6f0bc4))
- `cjson.decode` parity for `/`-unescape and stray-comma rejection
  ([9325ca8](https://github.com/Dicklesworthstone/frankenredis/commit/9325ca8b15ce5f02cce63ef85b15d8a1bc35c95e))
- Pattern matcher supports `%1`-`%9` back-references
  ([e06af73](https://github.com/Dicklesworthstone/frankenredis/commit/e06af735cf2f415e0b01cb3deba286510ae5188f))
- `%b` balanced-match + capture validation
  ([622ae95](https://github.com/Dicklesworthstone/frankenredis/commit/622ae9530dc8e16d1eb6cdac1a4d6c8b65e51a25))
- `%f[set]` frontier matcher
  ([6d9f1cb](https://github.com/Dicklesworthstone/frankenredis/commit/6d9f1cb9c9a4f63e2adfa3e3a5da3d36d18b88d4))
- `redis.call`/`pcall` argument validation gets `ERR` prefix and packages for `pcall`
  ([471d20d](https://github.com/Dicklesworthstone/frankenredis/commit/471d20da39e5f5b1e7da2cf18f33aef93edd9c70))

### Lua lexer/parser parity (long brackets, error wording, suffix rejection)

The lexer and parser now emit upstream Lua 5.1 error messages and syntax tokens byte-for-byte:
`[=*[ ... ]=*]` long-string/comment brackets at any nesting level, canonical "malformed number
near" wording, rejection of parse-time suffix on literal primaries, `5.1.5`-accurate string-escape
semantics with near-suffix tracking for "escape sequence too large".

- Long-string/comment level brackets `[=*[…]=*]`
  ([e177aec](https://github.com/Dicklesworthstone/frankenredis/commit/e177aec24707e04deef28ffb236d810754fb95d6))
- Parser/lexer error wording matches upstream
  ([524286e](https://github.com/Dicklesworthstone/frankenredis/commit/524286e554d1de80f92de64b8675328654bdb61f))
- Lexer emits upstream `malformed number near` wording
  ([3bd4434](https://github.com/Dicklesworthstone/frankenredis/commit/3bd4434d48b6506d30d898372f1ed4b627233508))
- Reject parse-time suffix on literal primaries
  ([34fcdac](https://github.com/Dicklesworthstone/frankenredis/commit/34fcdac6558779ade0f4be6d09c1a52f93e11be1))
- Add near-suffix to Lua `escape sequence too large`
  ([18f435b](https://github.com/Dicklesworthstone/frankenredis/commit/18f435b53a8e0e60c56d3b9ecee97eaa006e71a8))
- Lua string escapes follow Lua 5.1.5 lexer semantics
  ([7dcdf4a](https://github.com/Dicklesworthstone/frankenredis/commit/7dcdf4ad877c41b6faadbd9423afb02e50c23a2b))

### Stream parity: exclusive bounds, MAXLEN, consumer handling

`XRANGE` / `XREVRANGE` / `XPENDING` / `XAUTOCLAIM` accept the `(N` exclusive-bound syntax from
Redis 6.2+. Trim semantics now respect node boundaries; `XADD LIMIT 0` mirrors the vendored
diagnostic quirk; `XGROUP CREATECONSUMER` accepts an empty `MKSTREAM` stream.

- `XRANGE`/`XREVRANGE`/`XPENDING`/`XAUTOCLAIM` accept `(N` exclusive bound
  ([44aaae4](https://github.com/Dicklesworthstone/frankenredis/commit/44aaae401fdc081a21e4adcb417e58981d61c60a))
- Stream trim must not bump `max-deleted-entry-id`
  ([853d548](https://github.com/Dicklesworthstone/frankenredis/commit/853d5489547529190f8047445fc1a96c1c88a575))
- `XTRIM`/`XADD MAXLEN ~` honors node-boundary trim semantics
  ([0ade722](https://github.com/Dicklesworthstone/frankenredis/commit/0ade7225f0586ca0d649316f256527abccf4b45f))
- Mirror vendored `XADD LIMIT 0` wording quirk
  ([9c5c652](https://github.com/Dicklesworthstone/frankenredis/commit/9c5c652ae87f8684f15639cab27f78dea4885c8f))
- Defer `XTRIM LIMIT` diagnostics to post-parse
  ([32f4953](https://github.com/Dicklesworthstone/frankenredis/commit/32f49530d1f965913397a30494822a6be4a72614))
- `XGROUP CREATECONSUMER` accepts an empty `MKSTREAM` stream
  ([c930da9](https://github.com/Dicklesworthstone/frankenredis/commit/c930da99fa2cd4d11eaf4e72ef01c9e2d52f3cc6))

### CONFIG alignment with vendored 7.2.4

Listpack encoding defaults realigned to upstream (`hash-max-listpack-entries` = 128;
`set-max-listpack-value` cap enforced on promotion). Three `CONFIG` keys absent in 7.2.4 are
dropped, including the 7.4-only `hide-user-data-from-log`. `CONFIG GET` wildcard emits both
`slave-*` and `replica-*` aliases.

- `hash-max-listpack-entries` default 128 matches upstream
  ([f4bbeb5](https://github.com/Dicklesworthstone/frankenredis/commit/f4bbeb549bd096a78204388fcb443d5cb0ebf789))
- Drop three `CONFIG` keys absent in vendored 7.2.4
  ([faadec3](https://github.com/Dicklesworthstone/frankenredis/commit/faadec32fd3a7edafe1ee43c0f54660e8143056a))
- Drop `hide-user-data-from-log` (Redis 7.4-only)
  ([09876d8](https://github.com/Dicklesworthstone/frankenredis/commit/09876d82bbb7fca85b1389d310986decb64f3860))
- Honor `set-max-listpack-value` cap on listpack→hashtable promotion
  ([8e1b01d](https://github.com/Dicklesworthstone/frankenredis/commit/8e1b01d6acc00b3ed51b9518ddccb50b56fcb7cc))
- Drop fr-only `list-max-listpack-{entries,value}`
  ([4425efe](https://github.com/Dicklesworthstone/frankenredis/commit/4425efe9b3be6f46d305f405e1f941575aec0301))
- `CONFIG GET` wildcard emits both slave/replica aliases
  ([d44f98e](https://github.com/Dicklesworthstone/frankenredis/commit/d44f98e058b3de8405d823cb0a6a582cb436d702))

### DEBUG subsystem completion + COMMAND INFO pinning

`DEBUG LOADAOF` lands for AOF reload simulation. `DEBUG SDSLEN` reports jemalloc-rounded `zmalloc`
plus over-alloc slack per vendored behavior. `COMMAND INFO` subcommand order is pinned to vendored
7.2.4, and `COMMAND GETKEYSANDFLAGS` emits the correct keyspec flags for 13 STORE/mutate commands
plus EVAL key access modes.

- Implement `DEBUG LOADAOF` subcommand
  ([12afe2a](https://github.com/Dicklesworthstone/frankenredis/commit/12afe2ad6b918f59f9e1b169431a3560dd39c5e0))
- `DEBUG SDSLEN` reports jemalloc-rounded `zmalloc` and over-alloc slack
  ([554a506](https://github.com/Dicklesworthstone/frankenredis/commit/554a5068a36cf4cd71f95d24e6629fd371d8b5fe))
- Pin `COMMAND INFO` subcommand order to vendored 7.2.4
  ([14f6da0](https://github.com/Dicklesworthstone/frankenredis/commit/14f6da0c1f81e37fd72fa93a4f4fc2319d18c8ac))
- Pin upstream keyspec flags for 13 STORE/mutate commands in `GETKEYSANDFLAGS`
  ([476818c](https://github.com/Dicklesworthstone/frankenredis/commit/476818c8be4f43be75e6b8d59f8e62b9e3a08e1e))

### Replication + INFO timing/visibility fixes

Replication propagation and `INFO replication` backlog offsets are now gated until the first
replica connects and AOF is enabled — eliminating spurious offset chatter on standalone primaries.
`FAILOVER` replica check is deferred until after the parse loop so syntax errors surface first.

- Gate replication propagation pre-first-replica + no AOF
  ([d042720](https://github.com/Dicklesworthstone/frankenredis/commit/d042720de0c9f86081c3a3edc68c60b2da5af77e))
- Suppress `INFO replication` backlog offsets pre-first-replica
  ([2e45164](https://github.com/Dicklesworthstone/frankenredis/commit/2e451640d88f06e3a1a2e5ce5adfbc1cf6fe40a2))
- Defer `FAILOVER` replica check until after parse loop
  ([af0c625](https://github.com/Dicklesworthstone/frankenredis/commit/af0c6253b6532e866f2e39de32f1c01c3a1fc65c))
- Drop trailing CRLF after RDB bulk in `SYNC` reply
  ([5ddeae1](https://github.com/Dicklesworthstone/frankenredis/commit/5ddeae1add25ced3d35d6e54f17eeb48ccf11f59))

### Probe-sweep tail: bounds guards, type checks, encoding stability

A long tail of probe-sweep fixes: hash/zset encoding promotion becomes sticky across operations;
inverted-bound DoS vulnerability in `ZCOUNT`/`ZREVRANGEBYSCORE` patched; `ZPOPMIN`/`SPOP` `count=0`
on a wrong-type key returns `WRONGTYPE` instead of silently succeeding; `parse_f64` rejects
whitespace-padded stored values; LZF wire format matches vendored byte-for-byte.

- Guard `ZCOUNT` / `ZREVRANGEBYSCORE` inverted bounds (was DoS-able crash)
  ([98f8e08](https://github.com/Dicklesworthstone/frankenredis/commit/98f8e0834b8a7b0f57f3a9c5595f58a1f3a85fd4))
- `ZPOPMIN` `count=0` routes through type check
  ([0fea123](https://github.com/Dicklesworthstone/frankenredis/commit/0fea123a66ee5c9f5b9b1819a8e9e0de61e0ba79))
- `SPOP` `count=0` on wrong-type key reports `WRONGTYPE`
  ([9194d1b](https://github.com/Dicklesworthstone/frankenredis/commit/9194d1b60fd8cfb4de9ebd3c34fc80c40c87a35f))
- Hash/zset encoding promotion is now sticky
  ([2f7aeda](https://github.com/Dicklesworthstone/frankenredis/commit/2f7aedae8a5b40b007e71d1efd9ed1b74b5d6ff5))
- `parse_f64` rejects whitespace-padded stored values
  ([88ec823](https://github.com/Dicklesworthstone/frankenredis/commit/88ec823f7c5fac1de5eb888d41dd57e39e10c9c4))
- LZF wire format matches vendored byte-for-byte
  ([f816ddb](https://github.com/Dicklesworthstone/frankenredis/commit/f816ddb2d00c8a74fe8ac2d77c3bac048afc0fd0))

---

## Phase 10 -- Sentinel Subsystem, RDB Upstream Parity, Live Oracle Harness (2026-04-16 .. 2026-04-30)

The largest single-window in the project's history (~921 commits). A brand-new `fr-sentinel` crate
delivers Redis Sentinel monitoring/failover; RDB encoding gains byte-for-byte parity with vendored
Redis 7.2.4 including LZF compression, compact type tags, and FUNCTION DUMP envelopes; and the
live differential-oracle harness — running the same fixture suite against both runtimes and
diffing replies — is extended to most command domains.

### New crate: `fr-sentinel`

A clean-room reimplementation of Redis Sentinel arrives as its own crate with Phase 1–4
architecture: core types, periodic health checks, `__sentinel__:hello` pub/sub discovery,
quorum-based `O_DOWN` consensus with epoch-based leader voting, and a 7-state failover state
machine that selects a slave by priority/offset/runid, sends `REPLICAOF NO ONE`, and reconfigures
remaining replicas to the promoted master. The `SENTINEL` command set wires through this state
when the runtime is in sentinel mode.

- `fr-sentinel` crate with core types and `SENTINEL` command dispatcher
  ([1a65b4c](https://github.com/Dicklesworthstone/frankenredis/commit/1a65b4cd954777957a4fc9511f7d05fdea73c07d),
   [c2d670d](https://github.com/Dicklesworthstone/frankenredis/commit/c2d670d9d01a7b59b2623716c802bddf8a992b17))
- Phase 2 health checks + consensus module
  ([6bd42ae](https://github.com/Dicklesworthstone/frankenredis/commit/6bd42ae6264c66aad00b7abda9298f9c29111aba))
- Phase 3 pub/sub discovery
  ([ae98360](https://github.com/Dicklesworthstone/frankenredis/commit/ae983605d329c157c08b30bd761e691e65b0506c))
- Phase 4 failover state machine
  ([d481acd](https://github.com/Dicklesworthstone/frankenredis/commit/d481acd189e2d347577a1bcbbcc5d1e80d438e43))
- Golden-artifact tests for `HelloMessage` + `ReplicaInfo` parsing
  ([0eed0c6](https://github.com/Dicklesworthstone/frankenredis/commit/0eed0c6a12b8e934fcf18602e886fa868eed04f6))
- Fuzz corpus + contract tests for sentinel parsers
  ([7eb6524](https://github.com/Dicklesworthstone/frankenredis/commit/7eb6524cb6c4ffcb2b82826394b6a9fdf2d604d3),
   [9aaabdf](https://github.com/Dicklesworthstone/frankenredis/commit/9aaabdf49429dfe9360366f6dddd7ebe570ce1bc))
- Sentinel voter counting + `O_DOWN` cleanup
  ([fd3cfa0](https://github.com/Dicklesworthstone/frankenredis/commit/fd3cfa0031573db332434512d4ae59943022697b),
   [44909c8](https://github.com/Dicklesworthstone/frankenredis/commit/44909c887582ca3c7e44b8001449b36fd5158bfe))

### RDB upstream encoding parity

`fr-persist` learns to emit and consume the exact RDB byte layout that vendored Redis 7.2.4
produces: LZF compression on strings >20 bytes, compact-type encoder selection driven by value
shape, decoding of upstream type tags 11/16/17/18/20 (listpack hashes/zsets and stream variants),
and a full RDB length+version+CRC64 envelope around `FUNCTION DUMP` payloads so they round-trip
through vendored servers.

- LZF compression on RDB save
  ([9dee1ef](https://github.com/Dicklesworthstone/frankenredis/commit/9dee1efdf539f514df81d4513cdfa2ab575e4eba))
- Compact-type encoder selection in `encode_rdb_with_options`
  ([eaecea1](https://github.com/Dicklesworthstone/frankenredis/commit/eaecea1c44cb87da1e806d8e7c92502eb46263b9))
- Decode upstream compact RDB tags 11/16/17/18/20
  ([ec6a274](https://github.com/Dicklesworthstone/frankenredis/commit/ec6a27433e7a6beb20e57c5ad6c9453d84f9c9c8))
- Hash field TTL RDB encoding + roundtrip
  ([25d6a64](https://github.com/Dicklesworthstone/frankenredis/commit/25d6a64f0f5c4d005d9452d8d487e973ed3070ce))
- `FUNCTION DUMP` wrapped in upstream RDB version + CRC64 envelope
  ([14f4811](https://github.com/Dicklesworthstone/frankenredis/commit/14f48116afc1292131fb588fb4c5fee9e742e86c),
   [9e0915f](https://github.com/Dicklesworthstone/frankenredis/commit/9e0915f2bbf3de7e9c50da82ab2868e3e6d4ddff))
- RDB round-trip fuzz + live corpus harvester
  ([eeea0ae](https://github.com/Dicklesworthstone/frankenredis/commit/eeea0ae3159755b8b7acae97560d09dc9d2332d6),
   [9c460ed](https://github.com/Dicklesworthstone/frankenredis/commit/9c460ed3141b4d2747baa16c9d59f40184f2a333))

### Live differential oracle harness

Self-spawning vendored-Redis oracles, manifest-driven orchestrators, and oracle diffs across most
command domains land. Field-ordering canonicalization keeps RESP3 map/set replies stable across
runs; an exemption-audit schema marks intentional divergences.

- Self-spawning live-oracle integration
  ([8318b5f](https://github.com/Dicklesworthstone/frankenredis/commit/8318b5f23e7a5c5ee81ab4fb1d6e80d05ce5c5f4))
- Canonicalize oracle field ordering + exemption audit schema
  ([a236378](https://github.com/Dicklesworthstone/frankenredis/commit/a236378a84f4a8bc4cf9c16b89e5078f0e4ed0e2),
   [77165c4](https://github.com/Dicklesworthstone/frankenredis/commit/77165c4e9f6e9e4b92e1eb923cde0a7c58bdc71f))
- Live oracle debug module + sentinel wire
  ([c185136](https://github.com/Dicklesworthstone/frankenredis/commit/c185136d273593bc9e1daaa4761b5bfc7f41d78a))

### DEBUG subsystem expansion

15+ `DEBUG` subcommands implemented and gated behind `enable-debug-command` (Redis 7.2 default-deny
policy): `CHANGE-REPL-ID`, `STRINGMATCH-LEN`, `STRINGMATCH-TEST`, `QUICKLIST-PACKED-THRESHOLD`,
`OBJECT`, `HTSTATS`, plus correct arity-check ordering across all subcommands.

- DEBUG infra + `CHANGE-REPL-ID`, `STRINGMATCH-LEN`, `QUICKLIST-PACKED-THRESHOLD`
  ([b52736f](https://github.com/Dicklesworthstone/frankenredis/commit/b52736f8e78770d3d3b64eccc334250a549872a3),
   [c400966](https://github.com/Dicklesworthstone/frankenredis/commit/c400966926725d5095db83bc10df1eea3073400b))
- DEBUG arity check ordering + `CLIENT CACHING` wording
  ([8377028](https://github.com/Dicklesworthstone/frankenredis/commit/8377028649efba81eca80b6a8ef19be3cc6c3c76),
   [077eb01](https://github.com/Dicklesworthstone/frankenredis/commit/077eb01ff6833fa6d2189912cc13985b371a8ac9))
- Gate DEBUG commands on `enable-debug-command`
  ([b32de7d](https://github.com/Dicklesworthstone/frankenredis/commit/b32de7dfd973d29f50bc5bf661514338ddf10445))
- `DEBUG OBJECT` + live oracle encoding canonicalizer
  ([f1ffbc0](https://github.com/Dicklesworthstone/frankenredis/commit/f1ffbc0a79845bae38763f152857c4b1f72e6c14))

### INFO section parity and RESP3 Map emission

`INFO` reaches full section parity with vendored 7.2.4: 21-field `server`, 12+ new `stats`, 6-field
`clients`, plus correct memory and persistence sections drawn from real `/proc/self/status` and
runtime counters. `CONFIG GET` and `HGETALL` emit a RESP3 Map when the client negotiated
`protocol_version=3`; `XINFO STREAM`/`GROUPS`/`CONSUMERS` likewise emit Map per-entry.

- INFO server/stats/memory/persistence/clients field alignment
  ([a091d39](https://github.com/Dicklesworthstone/frankenredis/commit/a091d3930904044956e2629c41049855f48758ca),
   [b015a32](https://github.com/Dicklesworthstone/frankenredis/commit/b015a32d82e3358fcbc558f550bfbfa3743b650e),
   [7ea1569](https://github.com/Dicklesworthstone/frankenredis/commit/7ea15696e73436d063c656c1781512d33bc2a8a1),
   [eaa105f](https://github.com/Dicklesworthstone/frankenredis/commit/eaa105f3b1bd8b58946c89e4c5c77e559dce82ab))
- `INFO uptime_in_seconds` (time-since-startup)
  ([6a52ed0](https://github.com/Dicklesworthstone/frankenredis/commit/6a52ed0764cf173ee1d52f561f3d9a3d7a970c18))
- `MEMORY` subcommand + `DOCTOR` / `LOLWUT` wire format
  ([9f59b0a](https://github.com/Dicklesworthstone/frankenredis/commit/9f59b0ab70a2496f55d2598766c3045a556374c9))
- `CONFIG GET` + `HGETALL` emit RESP3 Map
  ([a655529](https://github.com/Dicklesworthstone/frankenredis/commit/a655529d48ea2c18c0e7d85da8f90ead40c17236))
- `XINFO STREAM`/`GROUPS`/`CONSUMERS` RESP3 Map type
  ([6b6cbb8](https://github.com/Dicklesworthstone/frankenredis/commit/6b6cbb8e2641518be7740216b578ba82dd6286b7),
   [bcf55d2](https://github.com/Dicklesworthstone/frankenredis/commit/bcf55d2c231e16b2c664417281d8d6c4b75abd63))
- `XGROUP CREATE`/`SETID ENTRIESREAD` form + subcommand-syntax-error parity
  ([69e766b](https://github.com/Dicklesworthstone/frankenredis/commit/69e766bc5f0db4a5f36d1a0998e91ec6a3a4e937),
   [a4d98b0](https://github.com/Dicklesworthstone/frankenredis/commit/a4d98b0047b8c6f44d35a19302b2b0cc3dde9b67))

### Metamorphic testing expansion

12 metamorphic test harnesses land: string, hash, list, set, zset, bit operations, bitop,
hash-numeric, keys-advanced, encoding matrix, hash field TTL, and `MULTI`/`EXEC` linearizability.

- 12 metamorphic test harnesses
  ([1856dce](https://github.com/Dicklesworthstone/frankenredis/commit/1856dce4ac00ac56260346cd53435d82f5aee0b4),
   [3cbc4cf](https://github.com/Dicklesworthstone/frankenredis/commit/3cbc4cf49e856468834396e8674b77faa2dc378e),
   [11d5658](https://github.com/Dicklesworthstone/frankenredis/commit/11d5658886df12474fcade249b6805ab01cd0196))
- Encoding matrix tests + hash field TTL + `MULTI/EXEC` linearizability
  ([7c048ee](https://github.com/Dicklesworthstone/frankenredis/commit/7c048ee14c70483edb90c69f03f8d0896d8b0a49),
   [f700569](https://github.com/Dicklesworthstone/frankenredis/commit/f700569542c93c3e85f8484e8e87ae1e30c5063e),
   [431fa9d](https://github.com/Dicklesworthstone/frankenredis/commit/431fa9d2b826d4b6650995fc91800ac0d43ab754))

### Command parity sweep (100+ edge cases)

A second wave of arity/wording/option-parsing fixes covering `SCAN`/`SSCAN`/`ZSCAN` `NOVALUES`,
`BITFIELD`/`GETBIT` 4 GiB bit-offset enforcement, `XPENDING`/`XAUTOCLAIM`/`XCLAIM` arity, `LPOS`/
`SPOP`/`LPOP`/`RPOP` count wording, `ZADD`/`ZINCRBY` NaN wording, `ACL DRYRUN`/`CAT`/`SETUSER`
validation, `CONFIG SET` odd-arity/error-wrapper parity, and `CLUSTER FAILOVER`/`ADDSLOTSRANGE`
gate ordering.

- `SCAN`/`SSCAN`/`ZSCAN` reject `NOVALUES` + `FUNCTION LIST` wording
  ([6567425](https://github.com/Dicklesworthstone/frankenredis/commit/6567425d8fa8d0aae8e91db8ce6c9c1d5e0b9c9f))
- `BITFIELD`/`GETBIT` 4 GiB bit-offset enforcement + `BITPOS` missing-key short-circuit
  ([2b82270](https://github.com/Dicklesworthstone/frankenredis/commit/2b82270d4f0efed35cf0c9f8be3a2f5c0e6d3c1a),
   [e665dc6](https://github.com/Dicklesworthstone/frankenredis/commit/e665dc6b54efec1f7c5d2a8e5e9c3a7f1e5d2b8a))
- `CONFIG SET` odd-arity / error-wrapper parity + `maxmemory memtoull` suffix
  ([0c33abb](https://github.com/Dicklesworthstone/frankenredis/commit/0c33abb1f7d0a5b8c1e9f2d5e6a9b3c0f4e1d8a5),
   [20cc5fc](https://github.com/Dicklesworthstone/frankenredis/commit/20cc5fc7b0e5d2c8b4a1f9c3e7d1a5b9c3d8e2a),
   [0c7f12b](https://github.com/Dicklesworthstone/frankenredis/commit/0c7f12ba695d0e2abd4d4ae145530d38fd21df4c))

### Client tracking, ACL, and Lua surface refinements

`CLIENT LIST ID` accepts 0/negative, `CLIENT TRACKING BCAST` conflict ordering and wording match
vendored, ACL `LOG` clamps negative counts, ACL `SETUSER` hash-password and modifier wording match,
EVAL_RO/EVALSHA_RO use the correct command name in `WrongArity`, FUNCTION FLUSH/RESTORE wording
aligns, redis.call propagation appends script context, `EVAL` shebang with `flags=no-writes` is
honored (Redis 7+).

- `CLIENT LIST ID` 0/negative + `CLIENT KILL`/`LIST` wording
  ([df35eed](https://github.com/Dicklesworthstone/frankenredis/commit/df35eed4dc5d898529c86e38632cd79ae41e962b),
   [e007b0d](https://github.com/Dicklesworthstone/frankenredis/commit/e007b0d6261f18923624fdd110f4ab4297227787),
   [b480e77](https://github.com/Dicklesworthstone/frankenredis/commit/b480e77b2b974c585065621898215f93c8bfa5c4))
- `CLIENT TRACKING BCAST` conflict ordering + `CLIENT PAUSE`/`UNBLOCK` parity
  ([a2de6e3](https://github.com/Dicklesworthstone/frankenredis/commit/a2de6e3ba1d836097326fadedaa75238ea2bae49),
   [cb67c7e](https://github.com/Dicklesworthstone/frankenredis/commit/cb67c7e9350d3163a73c2b92b5711d1e2c136e24))
- ACL `LOG` negative-count clamp + `SETUSER` hash-password/modifier wording
  ([f91367e](https://github.com/Dicklesworthstone/frankenredis/commit/f91367e6d9a2f5b8c1e4d7a0f3c6b9e2d5a8c1f4))
- `redis.call` script-context propagation
  ([958afe9](https://github.com/Dicklesworthstone/frankenredis/commit/958afe9ba4d1e7f2c5a8b0d3e6f9c2b5e8a1d4c7))
- `EVAL_RO`/`EVALSHA_RO` `WrongArity` uses correct command name + shebang validation
  ([4359032](https://github.com/Dicklesworthstone/frankenredis/commit/4359032e8c3f6a9d2e5a8b1c4f7e0d3c6b9e2a5d),
   [19e0dc8](https://github.com/Dicklesworthstone/frankenredis/commit/19e0dc8fe2d5a8b1c4f7e0d3c6b9e2a5d8c1f4e7))

---

## Phase 9 -- Throughput Recovery and Parity Surge (2026-04-01 .. 2026-04-15)

This window contains the famous performance turnaround. Profile-guided work in the first ten days
of April closes the gap from ~1.3% of Redis throughput (the baseline captured on April 7) to
**79–99% of Redis on single-command workloads** and **~31% on `pipeline=16`** by April 9,
documented in `artifacts/optimization/throughput-gap/ISOMORPHISM_PROOF_LAZY_DIGEST.md`. A second
sweep (Phase 2 final, April 13) lands `HashMap` migration, write coalescing, and double-parse
elimination — see `artifacts/optimization/phase2-final/DELTA_REPORT.md` for the breakdown.

### Throughput-gap recovery (April 9)

Two targeted optimizations move FrankenRedis from a near-unusable baseline to within striking
distance of vendored Redis: lazy threat-event digests (the SHA256 input-digest in the policy
ledger was being computed on every command — now only on policy-violating commands) and an ACL
category short-circuit (precomputed category bitmasks instead of per-dispatch glob matching).

- Lazy threat-event digests + ACL category short-circuit
  ([b13af4d](https://github.com/Dicklesworthstone/frankenredis/commit/b13af4da6fcc18f70f0c89593bc83c16fa81e21b))
- ACL category lookup precomputation
  ([ff4bfc9](https://github.com/Dicklesworthstone/frankenredis/commit/ff4bfc9cc7cd10c537d438c0854f05ddd0803d1f))
- Avoid intermediate `String` allocations in RESP encoding
  ([2813053](https://github.com/Dicklesworthstone/frankenredis/commit/28130532d6b72137613d0cc9c20ecf280cd3db53))

### Phase 2 final optimization sweep (April 13)

Three structural changes complete the throughput recovery. Headline numbers after this sweep:
SET p1 = 69,583 ops/sec (73.7% of Redis), GET p1 = 75,481 ops/sec (82.8%), GET p16 = 450,374
ops/sec, MIXED p16 = 370,617 ops/sec.

- `HashMap` migration for O(1) key lookups
  ([a12f657](https://github.com/Dicklesworthstone/frankenredis/commit/a12f657354f7a26ad61e0dc046d0154e9fb53d15))
- Eliminate double frame parsing and coalesce writes
  ([7df2135](https://github.com/Dicklesworthstone/frankenredis/commit/7df213540779d6b39bf67629c8431ea02aa8dfa2))
- Allocator support (`jemalloc`/`mimalloc`) for evaluation
  ([ac20368](https://github.com/Dicklesworthstone/frankenredis/commit/ac20368a99d7dd75dad6368ee64869313a1f2cbf))
- Performance recovery summary docs
  ([1ff29f6](https://github.com/Dicklesworthstone/frankenredis/commit/1ff29f60aec993ae574102f551537aa43956cdd0))

### Replication parity and topology

Stale-replica gating with `min-replicas-to-write`/`min-replicas-max-lag` admission, replication
control behavior, standalone failover topology shifts, and replica socket re-registration for
backlog writes. `DEBUG DIGEST`/`DIGEST-VALUE` for cross-replica state verification.

- Stale-replica gating + min-replicas write admission
  ([f8e01cf](https://github.com/Dicklesworthstone/frankenredis/commit/f8e01cfb69e2a604f3ca49a84e9677680a0d871f),
   [cee4309](https://github.com/Dicklesworthstone/frankenredis/commit/cee43095143cfed0689ca8f311036b1983c0ace3))
- Replication control parity and sync behavior
  ([2aaa6cd](https://github.com/Dicklesworthstone/frankenredis/commit/2aaa6cd94621af8e52e682b12ccd10aaf76c4c75),
   [b3e7e31](https://github.com/Dicklesworthstone/frankenredis/commit/b3e7e3129c00d534c0a9a2143cd37733ef1e2f80))
- Standalone failover topology shift
  ([4a8097a](https://github.com/Dicklesworthstone/frankenredis/commit/4a8097a6ef8175dd9b8e46c56c54b3a5a9fc9f9b))
- `DEBUG DIGEST`/`DIGEST-VALUE` and replica offset tracking
  ([e13ffd6](https://github.com/Dicklesworthstone/frankenredis/commit/e13ffd66d70eceb3942f1be1423414d1e233f08b))
- Re-register replica sockets for `WRITABLE` after queueing replication writes
  ([1325e6c](https://github.com/Dicklesworthstone/frankenredis/commit/1325e6c5de283f98894ef71771d42c0ac163a1b0))
- Add CRLF to FULLRESYNC snapshot bulk and consume trailing CRLF
  ([d3b8809](https://github.com/Dicklesworthstone/frankenredis/commit/d3b8809456cdd6d8a8a2bb68a83a5ec6e31b234d),
   [c940bc1](https://github.com/Dicklesworthstone/frankenredis/commit/c940bc1f8f89e8e85b4beacf3cac75cc9f476e5f))
- Handle partial CONTINUE backlog reads and disconnections
  ([07ef7b7](https://github.com/Dicklesworthstone/frankenredis/commit/07ef7b74e78d22b3d18cf10a26b33a51ccb25bc3),
   [23a9eb6](https://github.com/Dicklesworthstone/frankenredis/commit/23a9eb6f0ca850fd9db74ea8f80b5eca46f32a8f))
- Recompute repl backlog window on `CONFIG SET`
  ([55c1cff](https://github.com/Dicklesworthstone/frankenredis/commit/55c1cff5f0c0a04d28e41cecc404b006d5dfb4db))

### Persistence and INFO telemetry

Stream consumer groups and PEL state now persist through RDB; LZF decoder hardens against OOM
and overflow; `INFO` wires instantaneous ops/sec, network counters, and error-reply counters; RSS
read from `/proc/self/status` for accurate memory reporting.

- Persist stream consumer groups and PEL state in RDB
  ([a26b63e](https://github.com/Dicklesworthstone/frankenredis/commit/a26b63e25365ed23e88e21490d7b957b2160ef8e))
- LZF OOM guard, overflow handling, UTF-8 preservation
  ([c5e4e82](https://github.com/Dicklesworthstone/frankenredis/commit/c5e4e82ad69ebd0e5dc55c1b50a3eec5d2a7fab2))
- Wire instantaneous ops/sec, network counters, error replies
  ([3f9ff4e](https://github.com/Dicklesworthstone/frankenredis/commit/3f9ff4ebebc76ca58cd20a26d8da8627022564de))
- Real RSS from `/proc/self/status` for memory reporting
  ([7543394](https://github.com/Dicklesworthstone/frankenredis/commit/7543394298b55526d22d116bbf84dd59fb1ac5cb5))

### Lua, command, and option-parsing refinements

Lua metatable / coroutine stubs, six data-structure edge cases corrected, hard limit on
`unpack()` table-explosion (OOM prevention), bitfield overflow clamp, and strict integer parsing
for SCAN/COUNT/LIMIT/ACL/SELECT.

- Lua metatable support + coroutine stubs + `string.byte` fixes
  ([7d65fa9](https://github.com/Dicklesworthstone/frankenredis/commit/7d65fa9a9072749246dae1a6f453a286a523a3b2))
- Correct six data-structure edge cases
  ([9d30ecf](https://github.com/Dicklesworthstone/frankenredis/commit/9d30ecf9e738be3da2aa0c3c81cf4514754dcef4))
- Reject Lua `unpack()` with huge result count to prevent OOM
  ([8856a15](https://github.com/Dicklesworthstone/frankenredis/commit/8856a158aad195e9baba995e1f9ddd7b82f121f3))
- Bitfield overflow clamp
  ([521d641](https://github.com/Dicklesworthstone/frankenredis/commit/521d641c7db13f95c0e789b466e71eea54a032d7))
- Strict integer parsing for count/limit/SCAN/ACL/SELECT
  ([4ea8771](https://github.com/Dicklesworthstone/frankenredis/commit/4ea87712b39fc8baa193d652f8ae82d16b8bf2e0),
   [acc2cac](https://github.com/Dicklesworthstone/frankenredis/commit/acc2cac2c91f3c3c35ba4db39b996ea9844e3b2e))

### Conformance harness expansion

Multi-client oracle harnesses for blocking commands and pub/sub, manifest-driven live oracle
orchestrator with matrix profiles, and ~40 new conformance fixtures.

- Multi-client blocking-command conformance harness
  ([4d8b5c9](https://github.com/Dicklesworthstone/frankenredis/commit/4d8b5c9cb32c6a45f9c7098373dfe92cdc59c3b5))
- Multi-client live-oracle parity for pub/sub and topology
  ([b0456ac](https://github.com/Dicklesworthstone/frankenredis/commit/b0456ac7a346a5fa0bfbf7ea14ceb68cd7a4d8d4))
- Manifest-driven live-oracle orchestrator with matrix profiles
  ([e80e77c](https://github.com/Dicklesworthstone/frankenredis/commit/e80e77cd74354e0ae3e5d186dae638ebb8d18232))
- LIST/ZADD/PEXPIRE/KEYS/LCS edge cases
  ([95942be](https://github.com/Dicklesworthstone/frankenredis/commit/95942be816c7b18ace23df710d8f044891c7b9ff))
- EXPIREAT/PEXPIREAT invalid-integer cases + emission on delete
  ([bfbd2b4](https://github.com/Dicklesworthstone/frankenredis/commit/bfbd2b491336fddbd579f5449dcd66d0c6581dc6))
- BITFIELD OVERFLOW + GETEX/GETDEL + SETRANGE/STRLEN edge cases
  ([5ad2554](https://github.com/Dicklesworthstone/frankenredis/commit/5ad2554f643c492eec6887ea646e5122d28098b2))

---

## Phase 8 -- Full Command Surface, Lua Closures, Replication Correctness (2026-03-22 .. 2026-03-31)

The final stub replacements bring FrankenRedis to 100% command coverage with no fallback errors;
Lua scripts gain proper lexical scoping via upvalue capture; and a batch of critical replication,
transaction, and AOF/dirty-counter correctness fixes lands.

### Complete command surface — all 241 commands with real implementations

- Complete Redis command surface — all 241 base commands present
  ([e1220cd](https://github.com/Dicklesworthstone/frankenredis/commit/e1220cd2e64cc5994fc933d21ea4cc96f7e76235))
- Replace all stubs with real implementations across command/runtime/server
  ([6bec496](https://github.com/Dicklesworthstone/frankenredis/commit/6bec496001f8ae6208691951ce8469e992231468),
   [3d1358c](https://github.com/Dicklesworthstone/frankenredis/commit/3d1358ca94242c51cde7f9bf5520ac4481381668))
- Add `GEORADIUS_RO`, `GEORADIUSBYMEMBER_RO`, and `SYNC` command support
  ([39ac9cf](https://github.com/Dicklesworthstone/frankenredis/commit/39ac9cf3d79e1a8c459fe6eb2f7e02ef4f32f937))
- Implement `COMMAND DOCS`, `COMMAND GETKEYSANDFLAGS`, and `ZADD XX` fixes
  ([3596398](https://github.com/Dicklesworthstone/frankenredis/commit/3596398c1f450a18ad147394fffb8405cf69204e))
- `COMMAND GETKEYSANDFLAGS` and SCAN count semantics
  ([bf0dabf](https://github.com/Dicklesworthstone/frankenredis/commit/bf0dabf6bb43bbf849e113ea0e2f9e6a71829a0d))

### Lua scripting: closures, upvalue capture, safety limits

- Lexical scoping via upvalue capture for Lua closures
  ([8623a44](https://github.com/Dicklesworthstone/frankenredis/commit/8623a4469308e426e5f7b24f4c70a522285532ee))
- Closure upvalue capture + `rawset` fix + conformance updates
  ([70c4f93](https://github.com/Dicklesworthstone/frankenredis/commit/70c4f932d724934fb7acfe8cae2805b76e6abaa3))
- Self-recursion for local Lua functions
  ([e8def32](https://github.com/Dicklesworthstone/frankenredis/commit/e8def3296230e1f32d95cc679ad5cc3d06715431))
- Iteration limits on while/repeat/for in Lua eval (anti-runaway)
  ([01dd64a](https://github.com/Dicklesworthstone/frankenredis/commit/01dd64ac9207fd57f8bd874a4f125534007aa5ce))
- Harden Lua `string.rep` / `format` with bounds + overflow guards
  ([855d379](https://github.com/Dicklesworthstone/frankenredis/commit/855d379967d8d26d3257a26515d7ff811b5e6040))
- `lua_to_resp` stops at first nil in table array (Redis-compatible)
  ([7859e94](https://github.com/Dicklesworthstone/frankenredis/commit/7859e9411b982ca2e0f57e271c98a134fb2897ce))

### Replication, transaction, AOF correctness

`WATCH` now uses a per-key modification counter for proper ABA detection; AOF data-loss fixes for
missing write commands and stream dirty-tracking; `ROLE` returns the actual replication offset;
`MULTI` validates command arity at queue time.

- `WATCH` ABA detection via per-key modification counter
  ([69dd0da](https://github.com/Dicklesworthstone/frankenredis/commit/69dd0daf57b1ef9bd6d186fbe5f8c5e82bc3b574),
   [f632c19](https://github.com/Dicklesworthstone/frankenredis/commit/f632c193833c7db4f80830a77224885e1bc7d1c0))
- AOF data loss: missing write commands and stream dirty-tracking
  ([bd54d17](https://github.com/Dicklesworthstone/frankenredis/commit/bd54d17fa4895e50dd190dc06c8973b5011ef904),
   [64229fb](https://github.com/Dicklesworthstone/frankenredis/commit/64229fb69305d2f0ee2148bbd86eed1b78ada3bb))
- Add `modification_count` to `Entry` for `WATCH` correctness + missing dirty increments
  ([43e152b](https://github.com/Dicklesworthstone/frankenredis/commit/43e152b279eae4381e08e0a79c5ba60b3e60db57))
- `MULTI` arity validation + arity checker + randomized eviction
  ([87e75f4](https://github.com/Dicklesworthstone/frankenredis/commit/87e75f487c57c3c5b0c010dc8a3459d7b3f0d01b))
- `MULTI` transaction queueing and `CONFIG SET` atomicity fixes
  ([2363e78](https://github.com/Dicklesworthstone/frankenredis/commit/2363e7893795ec753bb5266dee42b6e47060ef44))
- `ROLE` returns actual replication offset (was hardcoded 0)
  ([93f5eb5](https://github.com/Dicklesworthstone/frankenredis/commit/93f5eb5171f885e61e9ccd6188b16ef03b477f09))

### ACL and client management

- Granular per-command and per-category ACL permissions
  ([2517241](https://github.com/Dicklesworthstone/frankenredis/commit/2517241096a0832b7988a82d15afe7b1ae83fb87))
- ACL security fixes: privilege escalation and resetpass flaws
  ([7360021](https://github.com/Dicklesworthstone/frankenredis/commit/736002123f61a12affeec3e6bd4b34c5cb2e278b))
- Real `CLIENT PAUSE` with command blocking
  ([bdebac1](https://github.com/Dicklesworthstone/frankenredis/commit/bdebac155058cbdb52f5d34021ff68b06ae92967))
- `CLIENT UNBLOCK` with blocked-client tracking + `LATENCY` subcommand fixes
  ([449322d](https://github.com/Dicklesworthstone/frankenredis/commit/449322d9d6a99910b25a138af536b7231ba3ba6e))
- `CLIENT LIST TYPE/ID` filtering + fail-closed `TRACKING`/`CACHING`
  ([afb4385](https://github.com/Dicklesworthstone/frankenredis/commit/afb4385d578452e0d9da5d4e8f9f37e6267e2853))

### Runtime configurability and persistence wiring

- Convert `NUM_DATABASES` to runtime-configurable `Vec`
  ([578cd92](https://github.com/Dicklesworthstone/frankenredis/commit/578cd92a76aa8ecc1150d8e0549d256725c8c9ec))
- Wire `CONFIG GET/SET` for `client-output-buffer-limit` with `proto-max-bulk-len`
  ([c36f345](https://github.com/Dicklesworthstone/frankenredis/commit/c36f345a17c5adbc98fae53c47787e440053a1f4))
- Wire `repl-backlog-size` / `repl-timeout`
  ([12a66eb](https://github.com/Dicklesworthstone/frankenredis/commit/12a66ebc57109020577bb3fb694b26ff6ea329ef))
- Wire `client-query-buffer-limit` / `proto-max-bulk-len`
  ([6d35317](https://github.com/Dicklesworthstone/frankenredis/commit/6d3531755fdac1d399fca030943c47dd23cd8a2b))
- Wire `maxmemory-samples` + `SHUTDOWN` graceful exit
  ([45af7c4](https://github.com/Dicklesworthstone/frankenredis/commit/45af7c4397ecdbde7f0a52dcd2ed879fdec8902d))
- Wire `CONFIG SET maxclients` / `busy-reply-threshold`
  ([2412512](https://github.com/Dicklesworthstone/frankenredis/commit/2412512e83bd9e0e65f2cfd6e740056b0e65684d))
- Multi-section `INFO` output + save-timestamp tracking
  ([2de1f37](https://github.com/Dicklesworthstone/frankenredis/commit/2de1f37726b64fb888756a0b55d65dad9f94066d))

### Protocol, stream, and data-structure correctness

`RespFrame::Sequence` for multi-message pub/sub replies; inline parser returns RESP error for
unbalanced quotes; `DUMP`/`RESTORE` upgraded from CRC16 to CRC64; real `XINFO CONSUMERS` metrics
from the pending-entries list; `XPENDING IDLE` (Redis 6.2+).

- Add `RespFrame::Sequence` for multi-message pub/sub replies + adoption across runtime/Lua
  ([2518c72](https://github.com/Dicklesworthstone/frankenredis/commit/2518c72d0d507b0aeedb5fea7dbfb7f09d3a0c9d),
   [0ed2c6d](https://github.com/Dicklesworthstone/frankenredis/commit/0ed2c6d659e51a7366a364f4e1eebc872f5a45ff))
- Return RESP error for unbalanced quotes in inline commands
  ([41f5d9e](https://github.com/Dicklesworthstone/frankenredis/commit/41f5d9e9fb671334ad5c29248983be4f06ad66f0))
- Negative-end guard on `ZRANGEBYSCORE`/`ZREMRANGEBYRANK` range checks
  ([990e836](https://github.com/Dicklesworthstone/frankenredis/commit/990e83616c6246ac5c47e40d5e113397b9b9ae96))
- Trim whitespace from geo coordinate float parsing
  ([49a87e4](https://github.com/Dicklesworthstone/frankenredis/commit/49a87e48e5bdcbbf6d4365ae81de512c187618b8))
- Upgrade `DUMP`/`RESTORE` from CRC16 to CRC64
  ([7757232](https://github.com/Dicklesworthstone/frankenredis/commit/7757232227672ad167ef048d69e7987b96bd5e0d))
- Real `XINFO CONSUMERS` metrics from PEL
  ([433ff9e](https://github.com/Dicklesworthstone/frankenredis/commit/433ff9e6571fd44d4f77c891fc86a8209c2df5bb))
- `XPENDING IDLE` option (Redis 6.2+)
  ([92942f5](https://github.com/Dicklesworthstone/frankenredis/commit/92942f50d4d0cbd223f9772c70284b55a7f31b0c))
- Stream correctness + RDB type-tag fixes
  ([22879c2](https://github.com/Dicklesworthstone/frankenredis/commit/22879c21b4ab90cc6f2a6acc8b677fad7a261a03))

---

## Phase 7 -- Real Server Telemetry, Cross-Client Pub/Sub, Lua Hardening (2026-03-20 .. 2026-03-21)

### Pub/Sub cross-client message delivery

- Implement multi-client Pub/Sub registry in `ServerState` with global channel and pattern routing
  ([9f0b357](https://github.com/Dicklesworthstone/frankenredis/commit/9f0b357e23621ce36ab65ebcdbd02638b2a812ea))
- Wire SUBSCRIBE, UNSUBSCRIBE, PSUBSCRIBE, PUNSUBSCRIBE, PUBLISH through the real TCP server with
  cross-client fan-out
  ([c68fafd](https://github.com/Dicklesworthstone/frankenredis/commit/c68fafd139021c23cf4af6c5fa715fe05e0c9430),
   [40f6ebb](https://github.com/Dicklesworthstone/frankenredis/commit/40f6ebb4f1f931436ec886a630648e9aaff2123c))
- Enforce subscription-mode command restriction: only SUBSCRIBE/UNSUBSCRIBE/PSUBSCRIBE/PUNSUBSCRIBE/PING/RESET allowed while subscribed
  ([b510512](https://github.com/Dicklesworthstone/frankenredis/commit/b510512b26fba78b0a62c7847a2f23ef29ff3d24))
- Fix multi-channel subscribe/unsubscribe response format to match Redis wire protocol
  ([a5a5e3c](https://github.com/Dicklesworthstone/frankenredis/commit/a5a5e3cc3327876df8e0a43a1713857b3deb33de))
- Deterministic channel/pattern ordering in unsubscribe-all responses
  ([6b7e1ee](https://github.com/Dicklesworthstone/frankenredis/commit/6b7e1ee3346fdf7e80d35fa62af8d7543f1f29d1))
- Optimize pub/sub client lookup and active-expire key counting
  ([8005379](https://github.com/Dicklesworthstone/frankenredis/commit/8005379aea59c60f9a7ff799f4e2c6ed189276ff))

### Server telemetry and introspection

- INFO now reports real runtime statistics: `connected_clients`, `total_commands_processed`,
  `total_connections_received`, `used_memory`, `maxmemory_policy`, dirty counter, expires count
  ([b9a50d3](https://github.com/Dicklesworthstone/frankenredis/commit/b9a50d32c2890d88eecc05da39c7ec6b2143c853))
- INFO reports real `run_id`, `process_id`, `tcp_port`
  ([d59d4fd](https://github.com/Dicklesworthstone/frankenredis/commit/d59d4fd0002a7ae1c75d7071871f08fb0998c0b0))
- Implement `COMMAND LIST FILTERBY MODULE|ACLCAT|PATTERN`
  ([14c1f6a](https://github.com/Dicklesworthstone/frankenredis/commit/14c1f6ac60255c02b4510465227d11b32ee82468))
- `RESET` now clears pub/sub subscriptions
  ([14c1f6a](https://github.com/Dicklesworthstone/frankenredis/commit/14c1f6ac60255c02b4510465227d11b32ee82468))
- Add HELP subcommands to command families
  ([ec18e3d](https://github.com/Dicklesworthstone/frankenredis/commit/ec18e3d8bff2fdd91f7be292fa1907579daaf0c4))
- Improve subcommand error handling with `UnknownSubcommand` for CLUSTER fallback
  ([f2d22e3](https://github.com/Dicklesworthstone/frankenredis/commit/f2d22e3a154d2c5fb37903d7dc9146c536686cb9))

### Lua scripting hardening

- Harden Lua `table.insert`, `table.remove`, `table.sort`, `table.concat` with proper type checking and bounds validation
  ([f1816d5](https://github.com/Dicklesworthstone/frankenredis/commit/f1816d5660ae2789cf532be162fb259dba17c396),
   [30c050e](https://github.com/Dicklesworthstone/frankenredis/commit/30c050eae8936b70a414dd63faed2bc1a2df425a))
- Fix `next()` iteration over tables with non-sequential keys
  ([30c050e](https://github.com/Dicklesworthstone/frankenredis/commit/30c050eae8936b70a414dd63faed2bc1a2df425a))
- Fix `select()` to handle negative indices and reject zero index per Lua 5.1 spec
  ([616ea53](https://github.com/Dicklesworthstone/frankenredis/commit/616ea5314f667c151e3bbc50da6e9ad89ef5dcdf))
- Strict numeric argument validation for all Lua table library functions
  ([6169d70](https://github.com/Dicklesworthstone/frankenredis/commit/6169d700b38b9d16dfb6e30a245584348b06eb5b))
- Nested table assignment write-back and deterministic set ordering
  ([ed1a81d](https://github.com/Dicklesworthstone/frankenredis/commit/ed1a81d92fa5c78f81e78fed78f9d45f7b122df1))
- Use exact float equality in Lua runtime; hoist timestamp in event loop
  ([c20cc7f](https://github.com/Dicklesworthstone/frankenredis/commit/c20cc7f73dfefcb4082c682f5f174f46a47f7a3a))
- Expand Lua scripting fidelity with refined sorted set operations
  ([05fb7a1](https://github.com/Dicklesworthstone/frankenredis/commit/05fb7a1a5244b8fa74e7f20877d58211524447d9))

### Sorted set correctness

- Use `total_cmp` for sorted set score comparisons; reject infinity in increments
  ([cc36c39](https://github.com/Dicklesworthstone/frankenredis/commit/cc36c397d6056bbb3cc2013c6707f94ba7563d38))
- Negative-zero score canonicalization and non-finite blocking timeout rejection
  ([d3213f4](https://github.com/Dicklesworthstone/frankenredis/commit/d3213f47ce75bb5f19b08b9c7bf3770a90103b32))
- Fix ZADD dirty tracking and missing command mappings
  ([8a18664](https://github.com/Dicklesworthstone/frankenredis/commit/8a18664d5338f4f8b466ef05f4521fd3dfa7290c))

### Persistence and data operation fixes

- Enable TCP_NODELAY and fix session swapping bug in `check_blocked_clients`
  ([05733ec](https://github.com/Dicklesworthstone/frankenredis/commit/05733ec981c8efc50ccd1bb148ca56c01ac4a551))
- Make AOF replay fail-closed; abort MULTI on unknown commands during replay
  ([2542319](https://github.com/Dicklesworthstone/frankenredis/commit/254231949fd2c6655d12789683871b1e3e554908))
- Handle RDB eviction opcodes
  ([8005379](https://github.com/Dicklesworthstone/frankenredis/commit/8005379aea59c60f9a7ff799f4e2c6ed189276ff))
- Add RDB 64-bit length encoding support
  ([22797a3](https://github.com/Dicklesworthstone/frankenredis/commit/22797a360adb5ba13b9970488ab7954404a7e42d))
- Fix LTRIM and LRANGE index normalization for out-of-range indices
  ([22447c1](https://github.com/Dicklesworthstone/frankenredis/commit/22447c1772f6f3ff3aaea387e82ea4190387bd31))
- Correct INCRBYFLOAT infinity handling across XGROUP/XINFO paths
  ([346f4e0](https://github.com/Dicklesworthstone/frankenredis/commit/346f4e03a5fe373f0b896fde50745b789aaa1d17),
   [b9a50d3](https://github.com/Dicklesworthstone/frankenredis/commit/b9a50d32c2890d88eecc05da39c7ec6b2143c853))
- Fix HDEL borrow conflict; document MULTI dispatch order
  ([1bc699d](https://github.com/Dicklesworthstone/frankenredis/commit/1bc699da97a6e6aa5ba6efa79c6de6b25ad795a7))
- Timestamp refactoring across dispatch, server, and store layers
  ([8a18664](https://github.com/Dicklesworthstone/frankenredis/commit/8a18664d5338f4f8b466ef05f4521fd3dfa7290c))

---

## Phase 6 -- TCP Server, Blocking Infrastructure, Crash-Safe Persistence (2026-03-15 .. 2026-03-19)

### Standalone TCP server (`fr-server` crate)

- Add `fr-server` crate with standalone FrankenRedis server binary
  ([9e3d06a](https://github.com/Dicklesworthstone/frankenredis/commit/9e3d06aa128258c8d74014560c7234ae9f820d72),
   [ede9226](https://github.com/Dicklesworthstone/frankenredis/commit/ede9226375ec3531522c27f57b816deee93f677d))
- Implement `mio`-based non-blocking TCP event loop with RESP wire protocol
  ([b2746b0](https://github.com/Dicklesworthstone/frankenredis/commit/b2746b039ed36a6bf5ff08f896088523183ac433))
- Expand TCP server with connection handling, auth, multi-client session management
  ([df69fbf](https://github.com/Dicklesworthstone/frankenredis/commit/df69fbf9d83b71a355e8d85a38837b7c9ce9375e),
   [91e561b](https://github.com/Dicklesworthstone/frankenredis/commit/91e561b2890fb6cc32a6d1a09942575a8d31535c))
- Restructure sorted sets for server-level store sharing
  ([b2746b0](https://github.com/Dicklesworthstone/frankenredis/commit/b2746b039ed36a6bf5ff08f896088523183ac433))
- Update PubSub protocol, command execution, and AOF loading paths for server integration
  ([33d2f1f](https://github.com/Dicklesworthstone/frankenredis/commit/33d2f1f77ce6b40826fb6aa30539ecf12b057bae))

### Blocking command infrastructure

- Wire BLPOP/BRPOP/BLMOVE/BLMPOP/BRPOPLPUSH to blocking infrastructure with timeout handling
  ([e2d46cc](https://github.com/Dicklesworthstone/frankenredis/commit/e2d46cc3ccddb6d27d6b08d6113f3b349c08c94e),
   [a38e28a](https://github.com/Dicklesworthstone/frankenredis/commit/a38e28a19a51d33aef2179c943460742877fd125))
- Wire BZPOPMIN/BZPOPMAX/BZMPOP to blocking sorted-set operations
  ([4fd3e61](https://github.com/Dicklesworthstone/frankenredis/commit/4fd3e613f23a95b829978b901cb94d4602cf5765))
- Wire XREAD BLOCK and XREADGROUP BLOCK to blocking infrastructure
  ([05f9bb1](https://github.com/Dicklesworthstone/frankenredis/commit/05f9bb155edc61603194f39b16a4ba2f5c309fb5))
- Prevent command processing during blocking operations
  ([045ae0a](https://github.com/Dicklesworthstone/frankenredis/commit/045ae0a5c14126020289f7b81fc041f1520fbd85))

### Crash-safe persistence and integrity

- Ensure crash-safe AOF persistence with atomic file I/O and mtime tracking
  ([a38e28a](https://github.com/Dicklesworthstone/frankenredis/commit/a38e28a19a51d33aef2179c943460742877fd125))
- Add CRC64 integrity checks to RDB binary format
  ([4fd3e61](https://github.com/Dicklesworthstone/frankenredis/commit/4fd3e613f23a95b829978b901cb94d4602cf5765))

### ACL, configuration, and encoding

- Real memory reporting (`used_memory`) and list encoding config (`list-max-listpack-entries`, `list-max-listpack-value`)
  ([39469db](https://github.com/Dicklesworthstone/frankenredis/commit/39469db9c50afebf034176179df6c3a5c6b98082))
- Enforce ACL gating on all commands; add dirty-flag mutation tracking
  ([e05c7c3](https://github.com/Dicklesworthstone/frankenredis/commit/e05c7c39b95d6ff78968cd8d5fab544e24da86c8))
- Fix ACL gating, PSYNC boundary, Lua hash migration
  ([29f6f2c](https://github.com/Dicklesworthstone/frankenredis/commit/29f6f2c6bee0cd9c0d3dba6fc42b143e890cfd5d))
- Correct list OBJECT ENCODING thresholds
  ([e50e4be](https://github.com/Dicklesworthstone/frankenredis/commit/e50e4be60adbbb880e085b3870e485ebeac9497c))
- JSON control character escaping and legacy listpack sizing
  ([4817797](https://github.com/Dicklesworthstone/frankenredis/commit/4817797c77dea536de2eb917f85b98889f33dcb9))

---

## Phase 5 -- Architecture Split, Stream Maturity, Encoding Thresholds (2026-03-13 .. 2026-03-14)

### Runtime architecture refactor

- Split monolithic `Runtime` into `ServerState` + `ClientSession` for multi-client readiness
  ([01065d1](https://github.com/Dicklesworthstone/frankenredis/commit/01065d165200cedead1aef20cfee436af314221a))
- Move per-session auth state from `AuthState` to `ClientSession`
  ([ff8d62d](https://github.com/Dicklesworthstone/frankenredis/commit/ff8d62d463cc9d48c875dda96aba7d056bb25824))

### Stream command maturity

- XADD MAXLEN/MINID/NOMKSTREAM and XTRIM MINID trimming support
  ([be92f56](https://github.com/Dicklesworthstone/frankenredis/commit/be92f56b1608a3dde12a3c1c15b0e0500f3eadfb))
- XADD partial auto-ID (`ms-*`) for explicit timestamp with server-generated sequence
  ([7c36749](https://github.com/Dicklesworthstone/frankenredis/commit/7c36749bcea630a366525c85b3a853f3f300a533))
- XINFO STREAM FULL option with entries and group details
  ([6eece57](https://github.com/Dicklesworthstone/frankenredis/commit/6eece5752ae34aa715c62d943e30c09c90754114))
- Serialize stream consumer groups and XSETID in AOF output
  ([6e55a18](https://github.com/Dicklesworthstone/frankenredis/commit/6e55a18c5b6b845884d027b84e42305abfcb13de))
- Copy stream `last-generated-id` during COPY command
  ([551be7a](https://github.com/Dicklesworthstone/frankenredis/commit/551be7a9459c7e538314e1885f8035bab4b6f9ad))

### Encoding, config, and new commands

- Add configurable encoding thresholds for hash, set, and zset types
  ([e18584c](https://github.com/Dicklesworthstone/frankenredis/commit/e18584c23b315ca885387683edd3193559707673))
- Add BITFIELD_RO command, LRU idle-time tracking, CONFIG SET persistence
  ([8cf020a](https://github.com/Dicklesworthstone/frankenredis/commit/8cf020a1435049ccb30fa30efe44dec67c21aed2))
- Persist CONFIG SET runtime state
  ([162af03](https://github.com/Dicklesworthstone/frankenredis/commit/162af03a7e65155c95119441e03d0a2a51e6429d))
- Expand pub/sub command handling and conformance coverage
  ([2fcd44f](https://github.com/Dicklesworthstone/frankenredis/commit/2fcd44f6fc5cbbb5b5b0e6002636eb4787468771))

### Conformance expansion

- Add dedicated smoke tests for `core_strings` and `core_errors`
  ([1c86790](https://github.com/Dicklesworthstone/frankenredis/commit/1c8679060d676aca7078f8d75e15260129ef47e6))
- Add 21 HyperLogLog accuracy and edge-case tests
  ([7fb3cc2](https://github.com/Dicklesworthstone/frankenredis/commit/7fb3cc28d42d668515ab3a9003e9de68162f6e9e))
- Add 22 SCAN edge-case tests for empty collections and scores
  ([bf5d813](https://github.com/Dicklesworthstone/frankenredis/commit/bf5d813eaed06f569af805fc499a877bd2bcd480))
- Add 28 tests for DBSIZE, RANDOMKEY, OBJECT, COMMAND, DEBUG
  ([3b2b89c](https://github.com/Dicklesworthstone/frankenredis/commit/3b2b89c8ae1104e79570d35d549cf6239ecf87de))
- Add conformance fixtures for BITFIELD_RO, CONFIG SET/GET, and OBJECT
  ([a013da0](https://github.com/Dicklesworthstone/frankenredis/commit/a013da0d968abd383025eb5d87c45422289addf0))

---

## Phase 4 -- Massive Conformance Expansion, Lua Standard Library, Redis 7.2 Compatibility (2026-03-03 .. 2026-03-12)

### Conformance test count: ~1500 to 3577 cases

- Systematic multi-round fixture expansion across all 30 command families
- Key conformance milestones:
  - 2041 total cases
    ([64b7a44](https://github.com/Dicklesworthstone/frankenredis/commit/64b7a4475400a9d7ef37c85de520730666ec40ac))
  - 2302 cases
    ([fbb8fd2](https://github.com/Dicklesworthstone/frankenredis/commit/fbb8fd2df0a6a32c750d5094575daf86cc1e8dd2))
  - 2666 cases
    ([392fbb7](https://github.com/Dicklesworthstone/frankenredis/commit/392fbb7a7017cbf79b8102cc0f90b790eee5bbb7))
  - 3000+ cases
    ([38af4a5](https://github.com/Dicklesworthstone/frankenredis/commit/38af4a532d62b45cbca725ca76d859e603ae7dd1))
  - 3336 verified cases
    ([c9874cc](https://github.com/Dicklesworthstone/frankenredis/commit/c9874ccce7c69d02568e3139a47bd00a3f3acfce))
- 108 blocking command tests with timeout error message fixes
  ([820a772](https://github.com/Dicklesworthstone/frankenredis/commit/820a772b20e909ebf3275b9fa2b5c3f9a10603ee))
- 28 WRONGTYPE cross-type error tests
  ([ab82985](https://github.com/Dicklesworthstone/frankenredis/commit/ab829856563a045cba188d69281bad25caca02e9))
- 98 tests for under-covered string/zset commands
  ([c3f60e5](https://github.com/Dicklesworthstone/frankenredis/commit/c3f60e52eae0b99dbdedffa267004ae78e4bb699))
- 37 stream consumer group tests (XCLAIM/XAUTOCLAIM/XPENDING/XSETID/XACK)
  ([32f83a9](https://github.com/Dicklesworthstone/frankenredis/commit/32f83a99eb1c3c6e2f618cc15ac977e2243a0758))
- 16 OBJECT ENCODING tests for all data type encodings
  ([fd7167d](https://github.com/Dicklesworthstone/frankenredis/commit/fd7167d76df86f752b97ad74fb90e08ebaaf50fb))
- Replication tests expanded from 16 to 51; WAIT tests from 23 to 43
  ([2df3407](https://github.com/Dicklesworthstone/frankenredis/commit/2df340795a16cf58d63eb43e4641e56119a2e579),
   [09e280e](https://github.com/Dicklesworthstone/frankenredis/commit/09e280e5e82e883d61e4a07f7780f36aa39816b7))
- ACL suite expanded from 29 to 69 cases
  ([4267b05](https://github.com/Dicklesworthstone/frankenredis/commit/4267b05977715a5310d192afd7a55642a906f11b))
- Transaction suite expanded from 43 to 107; errors from 25 to 108; strings from 84 to 155
  ([64b7a44](https://github.com/Dicklesworthstone/frankenredis/commit/64b7a4475400a9d7ef37c85de520730666ec40ac))
- Add 20 HyperLogLog edge case tests and 28 connection tests
  ([2a108f0](https://github.com/Dicklesworthstone/frankenredis/commit/2a108f0e8d5f08341e4b5f0482f21721e045dcce),
   [f10a399](https://github.com/Dicklesworthstone/frankenredis/commit/f10a3996149e001cdd27313ad2f87da75397c1dc))
- Add 9 INFO section conformance tests to core_server
  ([47390bd](https://github.com/Dicklesworthstone/frankenredis/commit/47390bd4771555d875336ca9ae0c2b1cb6496ca5))
- Expand conformance suites: config, pubsub, hash, set, zset, generic, stream, list, server, expiry, geo, scripting, cluster, function, scan, sort
  ([e561a61](https://github.com/Dicklesworthstone/frankenredis/commit/e561a61e2271d5c12f588fc968095af4a8280872),
   [7014cb9](https://github.com/Dicklesworthstone/frankenredis/commit/7014cb9d93f09539ad456cc63b8109fdd70c5240),
   [8854324](https://github.com/Dicklesworthstone/frankenredis/commit/88543244096b037fb61354030b7643a9f461f745),
   [392fbb7](https://github.com/Dicklesworthstone/frankenredis/commit/392fbb7a7017cbf79b8102cc0f90b790eee5bbb7))

### Full Lua 5.1 standard library

- String pattern matching engine: `string.match`, `string.gmatch`, `string.gsub`, `string.find`
  with full pattern support (character classes, quantifiers, anchors, captures, sets)
  ([9c89d6d](https://github.com/Dicklesworthstone/frankenredis/commit/9c89d6d350e86bf2341a2f49d15077ccad7b4bcb))
- `string.format` with full width/precision/flags support
  ([050038d](https://github.com/Dicklesworthstone/frankenredis/commit/050038dffaae01a20d0e9c7fa65ac7f6f14354b8))
- `string.gmatch` iterator, `pcall`/`xpcall` error handling
  ([dc8db5d](https://github.com/Dicklesworthstone/frankenredis/commit/dc8db5d8affcea0f4be8236d2aa396abc366f9d9))
- `table.sort` custom comparator, `rawset` mutation fix
  ([0dfb2f1](https://github.com/Dicklesworthstone/frankenredis/commit/0dfb2f1d9b2ae07e1dc1cd5db0597d33e3e1784e))
- `table.sort`/`insert`/`remove` now mutate caller's variable correctly
  ([f126d1a](https://github.com/Dicklesworthstone/frankenredis/commit/f126d1ad92e86145d21d1eab03d53579ed472aaf))
- Math trig functions (`sin`/`cos`/`tan`/`asin`/`acos`/`atan`/`atan2`), `math.randomseed`, `xpcall`
  ([9f05567](https://github.com/Dicklesworthstone/frankenredis/commit/9f055675c5390ca6326b84b5733ca2e813106a9d))
- `math.log10`/`modf`/`frexp`/`ldexp`, `os.clock`, redis stubs (`replicate_commands`/`set_repl`/`breakpoint`/`debug` with REPL_* constants)
  ([cc0f4a1](https://github.com/Dicklesworthstone/frankenredis/commit/cc0f4a1193e757bda82097be7971bb1da9e46a17))
- Additional string and table operations
  ([b70dce1](https://github.com/Dicklesworthstone/frankenredis/commit/b70dce1b05e4b310f13befd15e4818fb7c364819))
- Fix FCALL execution by transforming `register_function` into callable Lua
  ([c5cbcab](https://github.com/Dicklesworthstone/frankenredis/commit/c5cbcabff936251d144c2dce8326fd7e36bf2a2b))
- Specific error messages for FCALL/EVAL numkeys validation
  ([1b5714b](https://github.com/Dicklesworthstone/frankenredis/commit/1b5714ba0138140fe1f697c87c04027ae3035838))

### Redis 7.2 compatibility

- Full Redis 7.2-compatible HELLO response with client identity fields
  ([e333d50](https://github.com/Dicklesworthstone/frankenredis/commit/e333d50d0f6264e5df04654ddbb981b9ab794a68))
- HELLO with no args and SETNAME option
  ([2f60fb9](https://github.com/Dicklesworthstone/frankenredis/commit/2f60fb9df8353f09dde0e78be73eae2ca8b079cc),
   [ddd7524](https://github.com/Dicklesworthstone/frankenredis/commit/ddd7524e3625a3973dfd6b06ff43e81103651e5e))
- CLIENT SETINFO for Redis 7.2+ library identification
  ([000dc73](https://github.com/Dicklesworthstone/frankenredis/commit/000dc73ed1e5cee5992293f80edd5093dc99359f))
- CONFIG GET with multiple patterns (Redis 7+)
  ([1cb9170](https://github.com/Dicklesworthstone/frankenredis/commit/1cb9170a89739f948a1f60f36bb1656756cdc208))
- Full glob matching for CONFIG GET patterns
  ([3c6d310](https://github.com/Dicklesworthstone/frankenredis/commit/3c6d3108d3a056caa44eaf7d3deb37d2c97839af))

### Server introspection and command features

- Real SLOWLOG tracking with timing measurement and CONFIG integration
  ([c565b38](https://github.com/Dicklesworthstone/frankenredis/commit/c565b38636e05a202862a2d4c946297e2ace0d92))
- COMMAND GETKEYS with key extraction from COMMAND_TABLE metadata
  ([35848b9](https://github.com/Dicklesworthstone/frankenredis/commit/35848b97b5801f26998d3275928b20e96a2f0c1f))
- GEORADIUS/GEORADIUSBYMEMBER STORE/STOREDIST support
  ([e20d87b](https://github.com/Dicklesworthstone/frankenredis/commit/e20d87b58a011eee40a183328023d9a802fd2247))
- Full CLUSTER subcommand dispatch: INFO, MYID, SLOTS, SHARDS, NODES, KEYSLOT, GETKEYSINSLOT, COUNTKEYSINSLOT, RESET
  ([7b00450](https://github.com/Dicklesworthstone/frankenredis/commit/7b00450d924e8048073745736356ae0e45a592af))
- Real GETKEYSINSLOT and COUNTKEYSINSLOT with keyspace scanning
  ([0003376](https://github.com/Dicklesworthstone/frankenredis/commit/0003376120c0ac8ad07de754b4c125521c09e581))
- AOF rewrite pipeline, SORT conformance suite, intset encoding
  ([9dca209](https://github.com/Dicklesworthstone/frankenredis/commit/9dca2098523608150534ba3a6e3b5a2293f3ef31))
- Persist CONFIG SET `hz` with validation conformance tests
  ([3368409](https://github.com/Dicklesworthstone/frankenredis/commit/33684091cdbaad7070e6743e027693ba35b53c11))
- Add FLUSHALL as proper CommandId
  ([acec1a3](https://github.com/Dicklesworthstone/frankenredis/commit/acec1a3d70607fc58a20e4dd7e590a8fcde98079))

### Bug fixes and Redis parity corrections

- ZRANGE REV rank-mode correctness (now uses descending index like Redis) and BITOP validation
  ([b7fb8a6](https://github.com/Dicklesworthstone/frankenredis/commit/b7fb8a6edbca9eb63a319032cd4d2d35f657a6d8))
- Reject SET/GETEX with zero/negative expire times; add 40 edge-case tests
  ([2fc29dc](https://github.com/Dicklesworthstone/frankenredis/commit/2fc29dc32cb00138de0356e7282ec8fa136c9e10))
- Allow INCRBYFLOAT/HINCRBYFLOAT with infinity, reject only NaN
  ([c11137d](https://github.com/Dicklesworthstone/frankenredis/commit/c11137ddd3c85dfbe20c1bd2bda57bc03eaa6a55))
- Classify MSETNX, BRPOPLPUSH, BZPOPMIN/MAX, BZMPOP, ZDIFFSTORE as write commands
  ([fa11152](https://github.com/Dicklesworthstone/frankenredis/commit/fa1115244b040a7bbab83805ef6c6f7fc988189a))
- Return Redis-compatible error for non-positive numkeys
  ([9e08d06](https://github.com/Dicklesworthstone/frankenredis/commit/9e08d06f1a4584500ce44c00b035b00417f820ad))
- Reject empty RESP array frames in AOF record parsing
  ([2f98ed3](https://github.com/Dicklesworthstone/frankenredis/commit/2f98ed3ed898845d1384acdacb941e56a82fee77))
- Preserve stream consumer groups on COPY
  ([ab01233](https://github.com/Dicklesworthstone/frankenredis/commit/ab0123352ae5536a7d74be621f1f8d8a414f110f))
- Redis-accurate error messages for GETBIT and BITPOS
  ([11ee508](https://github.com/Dicklesworthstone/frankenredis/commit/11ee508e684c190f2c316665184aa499c2bee9a9))
- NOGROUP error format, XSETID persistence, bitmap error expectations
  ([67891fa](https://github.com/Dicklesworthstone/frankenredis/commit/67891fa70a902588c90cc34c27be36caa3fa86c8))
- Preserve destination TTL during PFMERGE (Redis parity)
  ([6a63fa4](https://github.com/Dicklesworthstone/frankenredis/commit/6a63fa4613ddeba10e078d74520643f032b5afd1))
- Lowercase command names in WrongArity error messages (Redis parity)
  ([ff7d02f](https://github.com/Dicklesworthstone/frankenredis/commit/ff7d02f534fc942083d68cff7cf859a38ffa5912))
- SETBIT error messages corrected
  ([0a63c3b](https://github.com/Dicklesworthstone/frankenredis/commit/0a63c3b438c0c16feda9ed1963b8581a2cf42164))
- Validate GEOSEARCH dual-center/shape, enforce subcommand arity
  ([5c18f64](https://github.com/Dicklesworthstone/frankenredis/commit/5c18f64ff193e480c8ea11a6b37779afc2635fb6))
- Accept XREAD/XREADGROUP BLOCK option instead of erroring
  ([56b88dd](https://github.com/Dicklesworthstone/frankenredis/commit/56b88dd19b35575d98f83d21880878c0a75c0e86))
- Respect `HarnessConfig.strict_mode` when selecting Runtime in conformance harness
  ([4f775df](https://github.com/Dicklesworthstone/frankenredis/commit/4f775dfb818540cde2a90f18a9be8da7e6a73329))
- Expiry evaluation boundary edge-case coverage
  ([42329df](https://github.com/Dicklesworthstone/frankenredis/commit/42329dfd71be18a1fd4668bae6a7a1cbc252b069),
   [ac4b509](https://github.com/Dicklesworthstone/frankenredis/commit/ac4b5091962b145d0e76afcc41c90321cf1defe5))

---

## Phase 3 -- Lua Scripting, Blocking Ops, DUMP/RESTORE, 215+ Commands (2026-02-25 .. 2026-02-26)

### Lua 5.1 scripting engine

- Full Lua 5.1 evaluator: variables, arithmetic, string concat, comparisons, logical ops,
  if/elseif/else, for/while/repeat loops, tables, function calls/definitions, `redis.call`/`pcall`,
  KEYS/ARGV
  ([a4c51e6](https://github.com/Dicklesworthstone/frankenredis/commit/a4c51e61f3ae50a9ded523e405154a59c589f7e9))
- EVAL/EVALSHA/EVAL_RO/EVALSHA_RO, SCRIPT LOAD/EXISTS/FLUSH
- FUNCTION subsystem (LOAD/LIST/STATS/DUMP/RESTORE/FLUSH/DELETE/HELP), FCALL/FCALL_RO
  ([5c23efe](https://github.com/Dicklesworthstone/frankenredis/commit/5c23efe2c6cbdda5b13e49f9f443ac749ea11885))
- Harden FCALL function name validation and fix FUNCTION RESTORE policy handling
  ([5c23efe](https://github.com/Dicklesworthstone/frankenredis/commit/5c23efe2c6cbdda5b13e49f9f443ac749ea11885))

### Blocking commands and Pub/Sub model

- BZPOPMIN/BZPOPMAX/BZMPOP implementation
  ([0af6c9f](https://github.com/Dicklesworthstone/frankenredis/commit/0af6c9f6e101e985281c0beba93f05e85f084086))
- Pub/Sub message model overhaul with multi-channel subscribe replies
  ([0af6c9f](https://github.com/Dicklesworthstone/frankenredis/commit/0af6c9f6e101e985281c0beba93f05e85f084086))
- Fix negative numkeys validation in BLMPOP/BZMPOP
  ([34c183a](https://github.com/Dicklesworthstone/frankenredis/commit/34c183a8e2fa637add08270942c040860343b3f3))

### Serialization and replication

- DUMP/RESTORE serialization with full type coverage
  ([3046ebb](https://github.com/Dicklesworthstone/frankenredis/commit/3046ebb4bf9afe33bde592dc5d5265451de935b1))
- Harden DUMP/RESTORE with CRC16 integrity and stream serialization; fix blocking command timeout validation
  ([14643e8](https://github.com/Dicklesworthstone/frankenredis/commit/14643e8db8fad4642b23ec2c9adc7c3ac2a63c27))
- REPLCONF and PSYNC command stubs for standalone mode
  ([2f5fb13](https://github.com/Dicklesworthstone/frankenredis/commit/2f5fb139052d1e9f69bcb886c7aa9969748fd4b9))
- Enforce strict PSYNC backlog bounds
  ([98cbb19](https://github.com/Dicklesworthstone/frankenredis/commit/98cbb19f680c85cfe01b4883931d87369dbbfdb2))
- Core replication conformance smoke test
  ([a2a7d6e](https://github.com/Dicklesworthstone/frankenredis/commit/a2a7d6e705f956509e85ac8a99e86727a03bf155))

### Command surface expansion to 215+

- ZADD options (NX/XX/GT/LT/CH), LPOS full (RANK/COUNT/MAXLEN), CLIENT/SLOWLOG/SAVE runtime commands, 8 new conformance suites
  ([b701186](https://github.com/Dicklesworthstone/frankenredis/commit/b701186f3bf1754778bc3b0b40ac7da6841ee57b))
- Expand dispatch to 140+ commands with geo, streams, pub/sub, blocking ops
  ([51f06cb](https://github.com/Dicklesworthstone/frankenredis/commit/51f06cb7cd44ddaad9e6328e43830e0822a884a6))
- Further expansion to 215+ with SORT, CLIENT, CONFIG, extended data ops
  ([a87b4e4](https://github.com/Dicklesworthstone/frankenredis/commit/a87b4e4c4d960655a545db69be7ce40341bc616d))
- CONFIG GET/SET command subsystem in `fr-runtime`
  ([72bf4d2](https://github.com/Dicklesworthstone/frankenredis/commit/72bf4d270c1425012be3861447abc3a7b2d029db))
- Core SET conformance fixture
  ([9a61c78](https://github.com/Dicklesworthstone/frankenredis/commit/9a61c784a074b2f23c6451047536a58d2a2f669b))

---

## Phase 2 -- Multi-Type Data Engine, Streams, Geo, ACL, Transactions (2026-02-19 .. 2026-02-22)

### Multi-type data engine

- Implement Hash, List, Set, Sorted Set data type stores and extended command handlers
  ([19046a1](https://github.com/Dicklesworthstone/frankenredis/commit/19046a17104831ad4c996ae235ef337e11f76710),
   [d9b72ad](https://github.com/Dicklesworthstone/frankenredis/commit/d9b72ad3f96e4462806e2bd1eff1cc6c59c4bb8d))
- GETEX, SMISMEMBER, SUBSTR, BITOP, ZUNIONSTORE, ZINTERSTORE
  ([0db1169](https://github.com/Dicklesworthstone/frankenredis/commit/0db1169070110b63121f63f0d0d35f2e9e8616c2))
- Server/connection commands and SCAN family (SCAN/HSCAN/SSCAN/ZSCAN)
  ([711691a](https://github.com/Dicklesworthstone/frankenredis/commit/711691ac78e79dbb2465325df7ed07d98deb4c9f))
- List manipulation commands with key expiry integration
  ([a40b757](https://github.com/Dicklesworthstone/frankenredis/commit/a40b757c565900eedd77e7ae452661bfc24e9ea8))
- Feature parity documentation updated to reflect 100+ commands
  ([8cc5825](https://github.com/Dicklesworthstone/frankenredis/commit/8cc58251b33da3c8a0fe8de60ba96cabbf8d0336))

### Transaction support

- MULTI/EXEC/DISCARD transaction support with memory enforcement
  ([9bb279c](https://github.com/Dicklesworthstone/frankenredis/commit/9bb279cec6f38d601df0791ff882f92a1b12a940))

### Redis Streams

- Core stream commands: XADD, XLEN, XDEL, XTRIM, XRANGE, XREVRANGE, XREAD
  ([cabb522](https://github.com/Dicklesworthstone/frankenredis/commit/cabb5223ea63aa8b2d184f824a48b5963a898dcc))
- Consumer groups: XACK, XCLAIM, XPENDING with pending entry tracking
  ([27ce066](https://github.com/Dicklesworthstone/frankenredis/commit/27ce066aa4e4d1e4b978650293fa94451cf421a2))
- XREADGROUP with consumer group read semantics
  ([561db47](https://github.com/Dicklesworthstone/frankenredis/commit/561db47716e54cc7bd492007c1ad2fe6e185a87c))
- XCLAIM, XAUTOCLAIM, XPENDING with replication handshake conformance
  ([cdae192](https://github.com/Dicklesworthstone/frankenredis/commit/cdae192a20d83b39bbb94884af756616fca1d78e))

### Geospatial commands

- GEOADD, GEOPOS, GEODIST, GEOHASH
  ([4b56db2](https://github.com/Dicklesworthstone/frankenredis/commit/4b56db2b53523cc25494d67e57b33eabff5df44a))
- Geo data type conformance suite and fixture
  ([41e9ef7](https://github.com/Dicklesworthstone/frankenredis/commit/41e9ef7916fa768501f2aeb0cf5bf0040906b568))

### ACL subsystem

- Full ACL command subsystem: AUTH, ACL SETUSER/GETUSER/DELUSER/LIST/WHOAMI/CAT/GENPASS/LOG
  with auth refactoring
  ([bbeeb57](https://github.com/Dicklesworthstone/frankenredis/commit/bbeeb578e8de40688531c63fffa60bf0a145e0dc))

### Correctness and safety fixes

- Reject NaN in float parsing, enforce GETEX PERSIST arity, fix ZSTORE weight/aggregate parsing
  ([fa5cd94](https://github.com/Dicklesworthstone/frankenredis/commit/fa5cd94a631e48ff3c7ef3df92e9d9c33f6bdb6e))
- Stream group cleanup on key removal, transaction memory enforcement, protocol hardening
  ([f775dd8](https://github.com/Dicklesworthstone/frankenredis/commit/f775dd81b9b8138744b0ed8d9f47b185ce4df84d))
- Harden KEYS glob matching, GETSET TTL preservation, command-dispatch edge cases
  ([5278918](https://github.com/Dicklesworthstone/frankenredis/commit/5278918f61e6bed538dcef7f9dd7503676316bf7))
- Replace `unwrap`/`unreachable` panics with proper error returns in all store operations
  ([e18fc79](https://github.com/Dicklesworthstone/frankenredis/commit/e18fc799b8217020c80bd9d944b2c6663f66146f))

### Conformance suites

- Add Hash, List, Set, Sorted Set fixture suites
  ([cb671f0](https://github.com/Dicklesworthstone/frankenredis/commit/cb671f0f2ea638be5ffc67c07df959223d4b788a))
- Persistence replay conformance fixtures
  ([abb20be](https://github.com/Dicklesworthstone/frankenredis/commit/abb20be030155e24884bac4fcb0207889b5e4532))
- Stream XREADGROUP and RESP protocol negative fixtures
  ([415db8a](https://github.com/Dicklesworthstone/frankenredis/commit/415db8a7928f4b6c90c5100e7a213dbeed331bc7))

### Tooling port: shell/Python to Rust

- Port 6 conformance orchestrators from shell/Python/jq to native Rust:
  live-oracle budget gate and bundle post-processing, RaptorQ artifact gate, benchmark round runner,
  adversarial triage, coverage budget runner
  ([6a33b51](https://github.com/Dicklesworthstone/frankenredis/commit/6a33b51d17571c7faaa8031f24bf9a793db030fd),
   [fe45110](https://github.com/Dicklesworthstone/frankenredis/commit/fe45110f80433968a4ce133fcbe9d9fdd82e661d),
   [31a44d7](https://github.com/Dicklesworthstone/frankenredis/commit/31a44d704de3923f8eebe6371d1cfff704a9a11a),
   [06e00cb](https://github.com/Dicklesworthstone/frankenredis/commit/06e00cb96948eeed5f841f1b5cafd45b6f570f3b),
   [a19a42c](https://github.com/Dicklesworthstone/frankenredis/commit/a19a42c666b450ccf5e15b7d397383d818e990e9),
   [c60b8bc](https://github.com/Dicklesworthstone/frankenredis/commit/c60b8bcece68105245e5cf7de5c59e0da2425f57),
   [378a595](https://github.com/Dicklesworthstone/frankenredis/commit/378a595615624eb7da5ab031e298e1aa8ea264cd),
   [952e477](https://github.com/Dicklesworthstone/frankenredis/commit/952e47758b6c31e69476d606054b9b8596614f2d))
- Enforce thin-shell wrapper contracts via integration tests
  ([879a489](https://github.com/Dicklesworthstone/frankenredis/commit/879a489842948df33dcad05df6021a254ad1d94f))

---

## Phase 1 -- Foundation: Protocol, Conformance Framework, Persistence, Replication (2026-02-13 .. 2026-02-18)

### Project bootstrap

- Clean-room workspace with 10 initial crates: `fr-protocol`, `fr-command`, `fr-store`, `fr-expire`,
  `fr-persist`, `fr-repl`, `fr-config`, `fr-conformance`, `fr-runtime`, `fr-eventloop`
  ([0376f60](https://github.com/Dicklesworthstone/frankenredis/commit/0376f609b01558c15cfd79ed6092f6d1efc718e6))
- MIT + OpenAI/Anthropic rider license
  ([cacc96d](https://github.com/Dicklesworthstone/frankenredis/commit/cacc96d1faa3208526dbe703ecbc4477bac0ca34))
- GitHub social preview image
  ([09ef57d](https://github.com/Dicklesworthstone/frankenredis/commit/09ef57d701899cc34d4fb969bd1a5fd4989d5434))

### RESP protocol and command dispatch

- RESP2 parser (`parse_frame`) and encoder (`RespFrame::to_bytes`) in `fr-protocol`
  ([0376f60](https://github.com/Dicklesworthstone/frankenredis/commit/0376f609b01558c15cfd79ed6092f6d1efc718e6))
- Distinguish unsupported RESP3 types from truly invalid prefixes
  ([96d8271](https://github.com/Dicklesworthstone/frankenredis/commit/96d82717f505ec7ce64d8c62765dac32ddcadc86))
- Harden RESP parser canonicalization; add optimization gate and negative fixture coverage
  ([bbe0585](https://github.com/Dicklesworthstone/frankenredis/commit/bbe0585268e924d481565dd67438123957373d93))
- 18 initial Redis string/key commands with glob matcher and conformance log validation
  ([ceb6e0e](https://github.com/Dicklesworthstone/frankenredis/commit/ceb6e0e8658223e977a948bea4efc8f060abd76e))
- Expand command dispatch with conformance fixtures
  ([c72dfb7](https://github.com/Dicklesworthstone/frankenredis/commit/c72dfb72c1babd56eee53a76fb25db853ec22116))

### Conformance harness and phase2c specification

- Fixture-driven conformance runner with strict/hardened runtime modes
  ([b2d40e0](https://github.com/Dicklesworthstone/frankenredis/commit/b2d40e0e60a2b76093473862f397ff41680c8c3b))
- Phase2c schema validation gate and artifact framework
  ([acb673a](https://github.com/Dicklesworthstone/frankenredis/commit/acb673ad60e8e14465f4504b4a58ba92b4cc7c6c))
- Phase2c conformance specification packets FR-P2C-001 through FR-P2C-009
  ([53b5eb2](https://github.com/Dicklesworthstone/frankenredis/commit/53b5eb257fa9fecc55a83bbb5aeb237507957c86),
   [6ba2f76](https://github.com/Dicklesworthstone/frankenredis/commit/6ba2f762d952d5822523a4fd8fd0fb6de0277ddf),
   [fb7d8e0](https://github.com/Dicklesworthstone/frankenredis/commit/fb7d8e0aa23a3cc3a2ecc910e5ea234d19ef9f6e),
   [b4f40f9](https://github.com/Dicklesworthstone/frankenredis/commit/b4f40f9521f4c78154b0a624ba743b08f27150d1),
   [eebf3e3](https://github.com/Dicklesworthstone/frankenredis/commit/eebf3e34867492fa06aa7175aab4bfc2b90cf1f1))
- Phase2c parity gates, fixture manifests, golden files, user journey corpus
  ([c3676e6](https://github.com/Dicklesworthstone/frankenredis/commit/c3676e6f38b4457e13c09b0004f787f931b8b774))
- FR-P2C-004 auth gate and ACL conformance tests
  ([7e0a979](https://github.com/Dicklesworthstone/frankenredis/commit/7e0a97949da43ee391cdf843930ec73bc45b23e9))
- FR-P2C-008 expire/evict differential conformance tests and user journey corpus
  ([060c280](https://github.com/Dicklesworthstone/frankenredis/commit/060c280cb5af609e7495de88e13b8d9b2ffe1277))
- Final evidence packs for FR-P2C-002, FR-P2C-003, FR-P2C-005, FR-P2C-006, FR-P2C-009
  ([8dab167](https://github.com/Dicklesworthstone/frankenredis/commit/8dab167677c96cedeeee8a635a86314aa0a8a1c2),
   [c532046](https://github.com/Dicklesworthstone/frankenredis/commit/c5320461111889a386429377aba38fb7bf09ce4b))
- FR-P2C-003 command dispatch core parity packet fixtures
  ([56829c8](https://github.com/Dicklesworthstone/frankenredis/commit/56829c8ef6cafaed7d7effc1fe04312953dbefef))
- Structured test log contract with golden fixtures
  ([b2d40e0](https://github.com/Dicklesworthstone/frankenredis/commit/b2d40e0e60a2b76093473862f397ff41680c8c3b))
- Event-loop contract adversarial test suite
  ([724863e](https://github.com/Dicklesworthstone/frankenredis/commit/724863e412b7d71bc7c6f23e11bbe189d74a8fde))
- Comprehensive edge-case and expiry contract tests
  ([2207d84](https://github.com/Dicklesworthstone/frankenredis/commit/2207d841a32f0a2270bbe89279d6cfa682c18709))

### Persistence and replication scaffold

- AOF record frame contract; integrate `fr-persist` AOF capture into command dispatch pipeline
  ([e705c1b](https://github.com/Dicklesworthstone/frankenredis/commit/e705c1b9f2c140f4817e0402cf241a3ff1653b53))
- Replication handshake FSM, PSYNC decision engine, WAIT/WAITAOF evaluators, AOF stream codec
  ([bf4c8f6](https://github.com/Dicklesworthstone/frankenredis/commit/bf4c8f66e8cac0ca5545d6002c1d82f4e896a821))
- WAIT and WAITAOF command surface with deterministic threshold evaluation
  ([be5b0f7](https://github.com/Dicklesworthstone/frankenredis/commit/be5b0f70a50ad51308af20281929c3ed487abb6d))
- Complete WAIT/WAITAOF invalid-arg unit matrix and conformance coverage
  ([82b0a1c](https://github.com/Dicklesworthstone/frankenredis/commit/82b0a1c9f674db6de29866c64c099fa694c8b7d6),
   [94c1ef5](https://github.com/Dicklesworthstone/frankenredis/commit/94c1ef5fd273012f214fb25570414a57ea3571f5),
   [2905de9](https://github.com/Dicklesworthstone/frankenredis/commit/2905de947ffbedb44319c668bcc2d8a5974efd40),
   [0262bc0](https://github.com/Dicklesworthstone/frankenredis/commit/0262bc0335d9149bad4c0a6e934e4f17118bb901),
   [9cd6956](https://github.com/Dicklesworthstone/frankenredis/commit/9cd695668febb1bf59b04291a9980b67244af462),
   [6d95ab6](https://github.com/Dicklesworthstone/frankenredis/commit/6d95ab639830a13d2b8c94ce25ac0dd6a2a451e7))

### Configuration and security

- Threat-class policy engine with strict/hardened deviation allowlists and bitmask file-presence checks
  ([a903a6e](https://github.com/Dicklesworthstone/frankenredis/commit/a903a6e82bfdc06273d814823bfc3a7764c87547),
   [46026ae](https://github.com/Dicklesworthstone/frankenredis/commit/46026ae65b55c5b257827e10b1a84ace907fc5c3))
- Security/compatibility threat matrix documentation
  ([63cf9ee](https://github.com/Dicklesworthstone/frankenredis/commit/63cf9ee2be464c6b3b20fc02ffe56dc9ddb11941))
- TLS configuration subsystem and runtime hot-apply engine
  ([0dab746](https://github.com/Dicklesworthstone/frankenredis/commit/0dab746922ad70fb6c97390413ec66e92b428c66))
- TLS runtime conformance tests for strict/hardened policy contracts
  ([ca9a31b](https://github.com/Dicklesworthstone/frankenredis/commit/ca9a31bda77561f07d2eb5689bd7cb8d14140b7b))

### Event loop and runtime

- Deterministic event loop planning with structured log contracts and tick budgets
  ([0b117f3](https://github.com/Dicklesworthstone/frankenredis/commit/0b117f303dea67fbc14a746c45b24b3683c03d6e))
- Event loop tick budget expansion and TLS runtime integration
  ([2634769](https://github.com/Dicklesworthstone/frankenredis/commit/2634769ee56642896cfe29bd17ac890b17383c40))
- User journey corpus gate, eventloop orchestration, and coverage flake budget
  ([6bb5e8d](https://github.com/Dicklesworthstone/frankenredis/commit/6bb5e8ddf787d7f64facfde3276019af91807c7b))
- Replay commands, artifact refs, and failure envelopes in diagnostics
  ([332bb45](https://github.com/Dicklesworthstone/frankenredis/commit/332bb455d5923b4b38042cd27d5ae2e98b515c45))

### Runtime commands

- CLUSTER, ASKING, READONLY, READWRITE handlers
  ([95d18e6](https://github.com/Dicklesworthstone/frankenredis/commit/95d18e63029bdc4523a8895c178a121bc5fd2236))
- PEXPIRE, EXPIREAT, PEXPIREAT, EXPIRETIME, PEXPIRETIME
  ([6057451](https://github.com/Dicklesworthstone/frankenredis/commit/6057451f306fe9d6a1915fcbf41a5e7188374657))
- Expand EXPIRE/PEXPIRE option compatibility tests to match Redis semantics
  ([13144b2](https://github.com/Dicklesworthstone/frankenredis/commit/13144b274a9c587895db6daa371491957614a625))

### CI and tooling

- Overhaul live conformance gates workflow with 8-gate topology, forensics indexing, structured artifact upload
  ([414f4cd](https://github.com/Dicklesworthstone/frankenredis/commit/414f4cd7d4e4d33f4605fd4613812cb73c97c0bb))
- Adversarial corpus expansion and dispatch journey fixture
  ([56eed41](https://github.com/Dicklesworthstone/frankenredis/commit/56eed4158d541c132f74e972069d9980a933930c))
- Fixture coverage expansion with replication/TLS journey files and contract table updates
  ([b67b159](https://github.com/Dicklesworthstone/frankenredis/commit/b67b159842fdcdaff5076dd7187f7d847b935007))

### Documentation passes

- DOC-PASS-00 through DOC-PASS-03: baseline matrices, crate cartography, symbol census, invariant catalog
  ([b0de028](https://github.com/Dicklesworthstone/frankenredis/commit/b0de0283e270bdda5cdfa14248f151970057e435))
- DOC-PASS-04: execution-path tracing and control-flow narratives
  ([6ae8922](https://github.com/Dicklesworthstone/frankenredis/commit/6ae89223621740f6ee48f25fff5e6d5f31430e39))
- DOC-PASS-07: error taxonomy, failure modes, and recovery semantics
  ([4ebe3c3](https://github.com/Dicklesworthstone/frankenredis/commit/4ebe3c3efa7ae2542333a461f8aa69f4670f2de7))
- DOC-PASS-11/12: behavioral semantics, oracle parity, and E2E orchestration
  ([2a3118e](https://github.com/Dicklesworthstone/frankenredis/commit/2a3118ee21f267c0ac97be2726f7a46716f5836e))
- DOC-PASS-00 baseline gap matrix with quantitative expansion targets
  ([8d772af](https://github.com/Dicklesworthstone/frankenredis/commit/8d772afd9b9e8018c8e7601f95a282086eb72e8a))

---

## Crate Map

| Crate | Role |
|---|---|
| `fr-protocol` | RESP2 parser and encoder (RESP3 downconversion via `ParserConfig::allow_resp3`); 2,067 LOC |
| `fr-command` | Command dispatch (232 distinct command names, zero stubs) + custom Lua 5.1 evaluator; ~85K LOC |
| `fr-store` | In-memory data engine: strings, hashes, lists, sets, sorted sets, streams, HLL, geo; hash field TTL storage + positional SCAN cursors |
| `fr-expire` | TTL evaluation (lazy + active-expire) |
| `fr-persist` | AOF record codec + RDB v11 (with LZF compression and upstream compact type tags 11/16/17/18/20/21) + standalone listpack decoder |
| `fr-repl` | Replication handshake FSM (`Handshake` → `FullSync` → `Online`), `PSYNC`/`SYNC` decisioning, `WAIT`/`WAITAOF` evaluator |
| `fr-config` | Strict/hardened mode policy engine, threat-class taxonomy, TLS configuration, encoding thresholds |
| `fr-conformance` | Fixture-driven differential harness against vendored Redis 7.2.4 (4,975 cases across 43 fixtures) + 13 oracle/orchestrator binaries |
| `fr-runtime` | `Runtime` orchestrator: `ServerState`, `ClientSession`, ACL, Lua, transactions, threat-event ledger, AOF/RDB capture |
| `fr-eventloop` | Deterministic event-loop planning with per-phase tick budgets |
| `fr-server` | Standalone `frankenredis` TCP server binary using `mio` (session swapping for blocking commands, replica socket lifecycle) |
| `fr-bench` | TCP benchmark harness (SET/GET/INCR/LPUSH/LPOP/HSET/HGET/MIXED workloads, HdrHistogram p50/p95/p99/p999) |
| `fr-sentinel` | Redis Sentinel reimplementation: `__sentinel__:hello` discovery, quorum-based S_DOWN/O_DOWN, epoch-based leader election, 7-state failover machine |

---

## Timeline Summary

| Date Range | Phase | Headline |
|---|---|---|
| 2026-02-13 .. 2026-02-18 | 1  | Foundation: 10 crates, RESP protocol, conformance framework, persistence scaffold, replication FSM, TLS, event-loop planning |
| 2026-02-19 .. 2026-02-22 | 2  | Multi-type data engine, streams, geo, ACL, transactions, 100+ commands, shell-to-Rust tooling port |
| 2026-02-25 .. 2026-02-26 | 3  | Lua 5.1 scripting, FUNCTION subsystem, `DUMP`/`RESTORE`, blocking ops, 215+ commands |
| 2026-03-03 .. 2026-03-12 | 4  | Conformance explosion (1,500 → 3,577 cases), full Lua stdlib, Redis 7.2 compat, `SLOWLOG`, `CLUSTER` |
| 2026-03-13 .. 2026-03-14 | 5  | Runtime split (`ServerState`+`ClientSession`), stream maturity, encoding thresholds, `BITFIELD_RO` |
| 2026-03-15 .. 2026-03-19 | 6  | TCP server with `mio`, blocking infrastructure (lists/sets/streams), crash-safe persistence, CRC64 |
| 2026-03-20 .. 2026-03-21 | 7  | Real `INFO` stats, cross-client Pub/Sub delivery, Lua hardening, sorted-set correctness |
| 2026-03-22 .. 2026-03-31 | 8  | All 241 commands real (zero stubs), Lua closures and upvalue capture, `WATCH` ABA correctness, RESP3 `Sequence` frames |
| 2026-04-01 .. 2026-04-15 | 9  | Throughput recovery (1.3% → 79–99% of Redis on p1, 31% on p16) via lazy threat digests + ACL short-circuit + HashMap store; Phase 2 final optimization sweep |
| 2026-04-16 .. 2026-04-30 | 10 | New `fr-sentinel` crate (monitoring + failover); RDB upstream encoding parity (LZF, compact type tags, FUNCTION DUMP envelope); live-oracle differential harness across most domains; DEBUG subsystem expansion; RESP3 Map emission |
| 2026-05-01 .. 2026-05-16 | 11 | Differential probe sweeps close the parity tail: Lua metamethod completion, sandbox-surface fill-in, lexer/parser wording match, stream exclusive bounds, `CONFIG` realignment to vendored 7.2.4, encoding-promotion stickiness |
