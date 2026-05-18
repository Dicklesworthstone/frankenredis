# FrankenRedis

<div align="center">
  <img src="frankenredis_illustration.webp" alt="FrankenRedis — memory-safe clean-room Redis reimplementation in Rust">
</div>

<p align="center">
  <a href="https://github.com/Dicklesworthstone/frankenredis/actions/workflows/live-conformance-gates.yml"><img alt="Live Conformance Gates" src="https://img.shields.io/github/actions/workflow/status/Dicklesworthstone/frankenredis/live-conformance-gates.yml?branch=main&label=conformance%20gates"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Rust 2024 (nightly)" src="https://img.shields.io/badge/rust-2024%20edition%20%2F%20nightly-orange">
  <img alt="Workspace 0.1.0" src="https://img.shields.io/badge/workspace-0.1.0-lightgrey">
  <img alt="Unsafe forbidden" src="https://img.shields.io/badge/unsafe-forbidden-success">
</p>

> **FrankenRedis is a memory-safe, clean-room Rust reimplementation of Redis 7.2.4 with strict drop-in protocol parity, a first-class strict/hardened compatibility split, a Sentinel state machine, and a 4,975-case differential conformance harness that diffs every reply against vendored Redis on every CI run.**

---

## TL;DR

### The problem

Redis is the de facto in-memory data store, but the canonical implementation is ~250k lines of C. That has real consequences:

- **Memory-unsafety risk surface.** Buffer overruns, use-after-free, signed-overflow UB, and parser quirks have produced real Redis CVEs over the years. A Rust reimplementation eliminates the entire category at compile time.
- **No first-class strict-vs-hardened compatibility split.** Stock Redis has one behavior. There is no way to say "be byte-exact with upstream" in one mode and "fail closed on anything ambiguous, log the decision, never bounded-defend silently" in the other.
- **Audit and replay opacity.** Stock Redis does not emit a structured, deterministic ledger of every threat-event, recovery, or policy override; reconstructing what happened after an incident is detective work.
- **No reusable internal model.** You cannot embed Redis's command engine, replication state machine, or RDB codec into a Rust project without bringing the whole server.

### The solution

FrankenRedis ships a Rust workspace whose crates each own one job (`fr-protocol`, `fr-command`, `fr-store`, `fr-persist`, `fr-repl`, `fr-sentinel`, `fr-runtime`, …) and a `frankenredis` binary that speaks RESP2/3 on the wire. Every component is `#![forbid(unsafe_code)]`. Every command goes through a strict-vs-hardened policy gate. Every divergence from vendored Redis 7.2.4 is either closed by a code change or filed as a tracked parity bead, gated by a differential conformance suite that runs the same fixtures against both servers and diffs the wire bytes.

### Why use FrankenRedis?

| You want… | FrankenRedis gives you |
|---|---|
| **A memory-safe Redis** | `#![forbid(unsafe_code)]` across 12 of 13 crates (parser, data engine, command dispatch, persistence, replication, sentinel, etc.); `fr-runtime` uses `#![deny(unsafe_code)]` with three audited `unsafe` blocks for `libc::waitpid` in BGSAVE/BGREWRITEAOF child-process supervision |
| **Drop-in protocol parity with Redis 7.2.4** | 241 commands implemented (zero stubs), 4,975 differential conformance cases run against the vendored Redis binary on every CI gate |
| **A compatibility/security policy split** | `Mode::Strict` (byte-exact replies, no defensive repairs) vs `Mode::Hardened` (preserves contract, adds fail-closed guards for malformed/adversarial input) |
| **A Sentinel state machine** | The `fr-sentinel` crate implements `__sentinel__:hello` discovery, quorum-based S_DOWN/O_DOWN, epoch leader election, and a 7-state failover machine, matching the conceptual model of Redis Sentinel. Currently exposed as a library and via `SENTINEL` command dispatch. |
| **Live differential parity testing** | The `fr-conformance` harness spawns vendored `redis-server`, executes the same fixture against both, and diffs replies byte-for-byte across 43 fixture families covering every command domain |
| **Reproducible benchmarks and a regression gate** | `fr-bench` + checked-in baseline JSON under `baselines/` + `scripts/benchmark_gate.sh` with tunable p99/throughput thresholds |
| **A clean architectural model to read or embed** | 13 small focused crates with explicit boundaries; the data engine, RESP codec, RDB codec, replication FSM, and Lua evaluator are each usable independently |

---

## See it in action

Start the server and talk to it with stock `redis-cli`:

```bash
# Terminal A — start FrankenRedis on the standard port
$ ./target/release/frankenredis --port 6379
[fr-server] listening on 127.0.0.1:6379 (mode=strict)

# Terminal B — drive it with the standard Redis client
$ redis-cli
127.0.0.1:6379> SET hello world
OK
127.0.0.1:6379> GET hello
"world"
127.0.0.1:6379> XADD events '*' kind login user 42
"1778889600123-0"
127.0.0.1:6379> XLEN events
(integer) 1
127.0.0.1:6379> EVAL "local v = redis.call('INCR', KEYS[1]); return v * tonumber(ARGV[1])" 1 counter 7
(integer) 7
127.0.0.1:6379> OBJECT ENCODING counter
"int"
127.0.0.1:6379> CLIENT INFO
id=3 addr=127.0.0.1:51294 laddr=127.0.0.1:6379 fd=12 name= age=42 idle=0 flags=N db=0 sub=0 psub=0 ssub=0 multi=-1 …
```

Wire up replication and persistence with the same flags Redis uses:

```bash
# Primary on 6379, replica on 6380, both AOF-enabled
$ ./target/release/frankenredis --port 6379 --aof ./primary.aof --rdb ./primary.rdb
$ ./target/release/frankenredis --port 6380 --aof ./replica.aof --rdb ./replica.rdb \
    --replicaof 127.0.0.1 6379
```

The replica issues `PSYNC` / `FULLRESYNC` against the primary, streams the RDB snapshot, replays the backlog, and acknowledges offsets via `REPLCONF ACK`, exactly like Redis does. You can also point a vendored `redis-server` at FrankenRedis (or vice versa) and they will replicate to each other.

---

## What's implemented today

| Area | State | Detail |
|---|---|---|
| **Commands** | **241 base commands, zero stubs.** | All command families: strings, hashes, lists, sets, sorted sets, streams, geo, hyperloglog, bitmap, pub/sub, scripting (EVAL/FCALL), server, cluster (single-node), connection, ACL, transactions, debug, function, memory, slowlog, latency, monitor. |
| **Wire protocol** | RESP2 native; RESP3 inbound parsing with downconversion. | `fr-protocol` enforces upstream `max_bulk_len = 512 MiB`, `max_array_len = 1M`, `max_recursion_depth = 128`, CRLF-injection sanitization on error/string bodies. RESP3 `Map`/`Set` emitted on the reply side when the client negotiates `protocol_version=3` (`HELLO 3`) for `CONFIG GET`, `HGETALL`, `XINFO STREAM/GROUPS/CONSUMERS`. |
| **Persistence** | AOF + RDB v11, both round-trip-tested. | Manifest-based multi-part AOF with `everysec`/`always`/`no` fsync; AOF replay with bounded tail-repair policies. RDB v11 with LZF compression on strings >20 B, CRC64 footer, standalone listpack decoder for upstream macro-node entries, and decoder coverage for upstream compact type tags `RDB_TYPE_SET_INTSET` (11), `RDB_TYPE_HASH_LISTPACK` (16), `RDB_TYPE_ZSET_LISTPACK` (17), `RDB_TYPE_LIST_QUICKLIST_2` (18), `RDB_TYPE_STREAM_LISTPACKS_2` (19), `RDB_TYPE_SET_LISTPACK` (20), and `RDB_TYPE_STREAM_LISTPACKS_3` (21). `FUNCTION DUMP`/`RESTORE` wrapped in an upstream version + CRC64 envelope so functions round-trip through vendored servers. |
| **Replication** | TCP end-to-end, both directions. | `--replicaof` and `REPLICAOF`/`SLAVEOF` from clients; `AUTH`/`REPLCONF`/`PSYNC` handshake; `FULLRESYNC` snapshot stream; `CONTINUE` backlog replay; `REPLCONF ACK` offset accounting; replica-of-replica chaining; `min-replicas-to-write` / `min-replicas-max-lag` write admission; reconnect with backoff. Integration tests prove legacy → FrankenRedis, password-protected legacy → FrankenRedis, and FrankenRedis → replica → downstream replica. |
| **Sentinel** | Library + `SENTINEL` command dispatcher. | `__sentinel__:hello` pub/sub discovery, periodic PING/INFO, subjective-down → quorum-voted objective-down with vote staleness filtering, epoch-based leader election, 7-state failover state machine (`SelectSlave` → `SendSlaveofNoone` → `WaitPromotion` → `ReconfSlaves` → `UpdateConfig`), and the full `SENTINEL` command surface. A dedicated launcher binary that puts the server into sentinel mode at startup (mirroring `redis-sentinel`) is still to come. |
| **Lua scripting** | Custom Lua 5.1 evaluator (no FFI, no embedded VM). | EVAL/EVALSHA/EVAL_RO/EVALSHA_RO + FCALL/FCALL_RO + FUNCTION LOAD/DUMP/RESTORE/STATS/DELETE; `redis.call`/`pcall`/`status_reply`/`error_reply`/`sha1hex`/`log`/`replicate_commands`/`set_repl`/`setresp`/`acl_check_cmd`/`breakpoint`/`debug` plus `redis.REPL_*` and `redis.LOG_*` constants; full pattern matcher (`%b` balanced, `%f[set]` frontier, `%1`–`%9` back-references); script-relevant metamethods (`__index`/`__newindex`/`__call`/`__concat`/`__add` family/`__eq`/`__lt`/`__le`/`__tostring`/`__unm`/`__metatable`; `__mode`/`__len`/`__gc` not exposed); LuaJIT-compatible `bit` library; `cjson.encode`/`decode` with upstream `%.14g` formatting; KEYS/ARGV; closures with lexical-scope upvalue capture; coroutines. |
| **ACL** | Full lifecycle. | `SETUSER`/`GETUSER`/`DELUSER`/`LIST`/`USERS`/`WHOAMI`/`CAT`/`GENPASS`/`LOG`/`SAVE`/`LOAD`/`DRYRUN`/`HELP`; per-command `+cmd`/`-cmd`, per-category `+@cat`/`-@cat`, `allcommands`/`nocommands`/`allkeys`/`allchannels`/`reset`, key pattern `~pattern`, channel pattern `&pattern`; deny-first precedence enforced at dispatch. |
| **Pub/Sub** | Cross-client delivery. | SUBSCRIBE/UNSUBSCRIBE/PSUBSCRIBE/PUNSUBSCRIBE/PUBLISH/PUBSUB plus shard variants SSUBSCRIBE/SUNSUBSCRIBE/SPUBLISH; subscription-mode command restriction; deterministic unsubscribe-all ordering. Keyspace notifications via `CONFIG SET notify-keyspace-events`. |
| **Blocking ops** | Real socket-level blocking. | BLPOP/BRPOP/BLMOVE/BLMPOP/BRPOPLPUSH/BZPOPMIN/BZPOPMAX/BZMPOP/XREAD BLOCK/XREADGROUP BLOCK/WAIT/WAITAOF/CLIENT PAUSE with deadline tracking and session swap-out in the mio loop. |
| **Conformance** | 4,975 fixture cases across 43 families, run differentially. | The `fr-conformance` harness can run a fixture against the FrankenRedis runtime in-process *and* against a live vendored `redis-server` over TCP, then diff replies byte-for-byte. `fr_p2c_*` packet suites cover event-loop, dispatch, ACL, replication, cluster, expire/evict, and TLS journeys. |
| **Fuzzing** | 33 `cargo-fuzz` targets. | Parser (RESP, inline), command parse, RDB encode/decode/structured, AOF decoder/manifest, store bitops/HLL/scan/stream-groups, runtime execute_bytes/sequence/eventloop validators, Lua eval, function restore, PSYNC reply, MIGRATE/DUMP, client tracking, keyspace events, glob match, config file, TLS config, ACL rules, sentinel parsers. |
| **Benchmarks** | Checked-in baselines + regression gate. | `crates/fr-bench` issues 8 standard workloads (SET/GET/INCR/LPUSH/LPOP/HSET/HGET/MIXED), records HdrHistogram p50/p95/p99/p999, normalizes to `frankenredis_baseline/v1` JSON, and `scripts/benchmark_gate.sh` compares candidate runs against the checked-in baselines with tunable thresholds. |

---

## Performance

Numbers below are from the standard `fr-bench` workload (50 clients, 100k requests, 10k keyspace, 3-byte payload) against FrankenRedis built with `--release`, with vendored Redis 7.2.4 as the comparison oracle on the same host. Raw JSON is checked in under `baselines/`, and the latest delta report is `artifacts/optimization/phase2-final/DELTA_REPORT.md`.

| Workload | Pipeline | FrankenRedis (ops/sec) | Redis 7.2.4 (ops/sec) | % of Redis | FR p50 (µs) | FR p99 (µs) |
|---|---|---|---|---|---|---|
| SET   | 1  |  69,583 |  94,402 | **73.7%** | 694 | 1,214 |
| GET   | 1  |  75,481 |  91,142 | **82.8%** | 641 |   975 |
| INCR  | 1  |  66,606 | ~94,000 | **~71%**  | 712 | 1,191 |
| LPUSH | 1  |  64,811 | ~85,000 | **~76%**  | 716 | 1,325 |
| LPOP  | 1  |  66,748 | ~85,000 | **~79%**  | 711 | 1,312 |
| HSET  | 1  |  61,233 | ~80,000 | **~77%**  | 750 | 1,392 |
| HGET  | 1  |  73,127 | ~90,000 | **~81%**  | 662 | 1,142 |
| MIXED | 1  |  71,012 | ~92,000 | **~77%**  | 681 | 1,089 |
| GET   | 16 | 450,374 | ~950,000| **~47%**  |   — |     — |
| MIXED | 16 | 370,617 | ~880,000| **~42%**  |   — |     — |
| SET   | 16 | 284,618 |  860,900| **~33%**  |   — |     — |

> Bench: 50 clients, 100k requests, 10k-key keyspace, 3-byte payload. Build: `cargo build --release -p fr-server`. Optional allocator features `mimalloc` or `jemalloc` may be enabled with `--features mimalloc`/`--features jemalloc`; the default is the system allocator.

**Reading these numbers.** Single-command throughput sits in the **71–83% range of vendored Redis 7.2.4** across the eight standard workloads (geometric mean ~77%), with median request latency well under a millisecond and p99 in the 1.0–1.4 ms range. Heavily pipelined batches (`pipeline=16`) show a wider gap: Redis benefits there from very aggressive batching and `writev` scatter-gather. The FrankenRedis pipeline path coalesces writes per-poll-cycle but does not yet use `writev`, and closing that gap is the next perf workstream. Run-to-run variance is non-trivial; individual workloads have hit higher parity numbers on warmer cache runs (e.g. `GET p1` has clocked 99% parity in prior captures).

To reproduce locally:

```bash
# Build, run both servers, capture normalized baseline JSON under baselines/
./scripts/record_baselines.sh

# Compare a candidate run against the checked-in baselines
FR_BENCH_THROUGHPUT_DROP_PCT=10 FR_BENCH_P99_REGRESSION_PCT=20 \
  ./scripts/benchmark_gate.sh
```

---

## Design philosophy

### 1. Deterministic Latency Replication Core (DLRC)

Strict command semantics, tail-aware scheduling, and recoverable persistence pipelines are treated as core identity constraints, not best-effort niceties. The event loop in `fr-eventloop` is planned in explicit phases (`BeforeSleep` → `Poll` → `FileDispatch` → `TimeDispatch` → `AfterSleep`) with per-phase tick budgets (`max_accepts`, `max_commands`) so that one client cannot starve another and so that replay/audit can reconstruct exactly what the loop did each tick.

### 2. Strict vs Hardened mode split

`fr-config` exposes two operating modes, set with `--mode strict` or `--mode hardened`:

| Mode | Posture | When to use |
|---|---|---|
| **Strict** | Maximizes observable compatibility with vendored Redis. No behavior-altering repairs. If the input is ambiguous, reproduce the upstream behavior, including upstream quirks. | Production drop-in replacement, anywhere a downstream client depends on byte-exact replies. |
| **Hardened** | Preserves the API contract but adds bounded defensive recovery for malformed inputs and hostile edge cases. Threat events are recorded; the decision (`FailClosed`, `BoundedDefense`, `RejectNonAllowlisted`) is logged with an input digest. | Internet-facing deployments, multi-tenant gateways, anywhere you'd rather reject than guess. |

The mode is enforced at the `Runtime::execute_frame` boundary, not as a sprinkling of `if` statements. Adding a new command means filing it under one of the eight threat classes in `fr-config::ThreatClass` (ParserAbuse, VersionSkew, ResourceExhaustion, …) and declaring the decision action for each class.

### 3. Differential conformance against the real oracle

There is one source of truth for command behavior: a vendored Redis 7.2.4 binary checked out under `legacy_redis_code/redis`. The `fr-conformance` harness runs every fixture against both FrankenRedis and the live `redis-server`, captures the wire bytes, and diffs them. If you can't write a fixture that the vendored binary produces a known reply for, the behavior isn't allowed to land.

A repeating "probe sweep" workflow uses adversarial command sequences to find new divergences, files each one as a beads issue, and closes them one at a time. This pattern is visible across hundreds of commits tagged `(frankenredis-<slug>)`.

### 4. Memory safety as a structural property, not a slogan

`#![forbid(unsafe_code)]` is set in 12 of 13 crates (the parser, data engine, command dispatch, Lua evaluator, persistence, replication, sentinel, event loop, config, expire, server, conformance, bench). The thirteenth, `fr-runtime`, uses `#![deny(unsafe_code)]` and contains exactly three audited `unsafe` blocks; all of them wrap `libc::waitpid` calls used to reap `BGSAVE` and `BGREWRITEAOF` child processes. The fork-and-wait pattern is unavoidable for those subsystems on Unix; everything else stays in safe Rust. Common dangerous helpers (`libc::getrusage`, `libc::clock_gettime`, raw FFI) are explicitly avoided. The custom Lua evaluator is written in safe Rust against a `LuaValue` enum, with no embedded C VM.

### 5. Audit-first, repair-second

When the hardened mode detects something unusual (an oversized bulk, an unknown RESP3 type, a malformed AOF tail), it emits a deterministic threat-event ledger entry with a SHA256 input digest, a class, a decision, and a timestamp. That ledger is what the operator sees in postmortem, not just "the server kept running."

### 6. Profile-guided optimization with isomorphism proofs

Performance work is gated on showing that observable behavior didn't change. Each round produces an `ISOMORPHISM_PROOF_*.md` artifact next to before/after flamegraphs and `strace` syscall profiles under `artifacts/optimization/`.

---

## Architecture

```
                        ┌──────────────────────────────────────────────┐
   TCP client  ────────▶│  fr-server (binary: `frankenredis`)          │
                        │  mio event loop, ClientConnection registry   │
                        │  - read_buf / write_buf per connection       │
                        │  - blocked-client deadlines, session swap    │
                        │  - replica socket lifecycle, backlog flush   │
                        └────────────────────┬─────────────────────────┘
                                             │  bytes
                                             ▼
                        ┌──────────────────────────────────────────────┐
                        │  fr-runtime :: Runtime                       │
                        │  ┌────────────────────────────────────────┐  │
                        │  │ execute_bytes → parse RESP             │  │
                        │  │ execute_frame:                         │  │
                        │  │   preflight policy (mode, ACL, mem)    │  │
                        │  │   active-expire pulse                  │  │
                        │  │   transaction state (MULTI/EXEC)       │  │
                        │  └────────────────────────────────────────┘  │
                        └─────┬─────────────┬────────────────┬─────────┘
                              │             │                │
              ┌───────────────▼──┐ ┌────────▼────────┐ ┌─────▼─────────┐
              │ fr-command       │ │ fr-store        │ │ fr-persist    │
              │ dispatch (231    │ │ data engine     │ │ AOF + RDB v11 │
              │ commands), Lua   │ │ + hash field    │ │ + LZF + CRC64 │
              │ 5.1 evaluator    │ │ TTL + SCAN      │ │ + listpack    │
              └───────────────┬──┘ └────────┬────────┘ └──────┬────────┘
                              │             │                 │
                              └─────────────┼─────────────────┘
                                            │
                              ┌─────────────▼──────────┐ ┌──────────────────┐
                              │ fr-expire :: TTL eval  │ │ fr-repl :: PSYNC │
                              │ (lazy + active cycle)  │ │ + WAIT/WAITAOF   │
                              └────────────────────────┘ │ + handshake FSM  │
                                                         └──────────────────┘

                        ┌──────────────────────────────────────────────┐
                        │  fr-sentinel  (library + SENTINEL dispatch)  │
                        │  __sentinel__:hello discovery                │
                        │  S_DOWN → quorum O_DOWN → epoch leader vote  │
                        │  7-state failover machine                    │
                        └──────────────────────────────────────────────┘
```

### Crate map

| Crate | Role |
|---|---|
| [`fr-protocol`](crates/fr-protocol) | RESP2 parser/encoder; RESP3 inbound parsing with downconversion; CRLF-injection sanitization; configurable bulk/array/recursion limits. |
| [`fr-command`](crates/fr-command) | Command dispatch (`dispatch_argv`) for 231 distinct commands and a custom Lua 5.1 evaluator (`lua_eval.rs`). Largest crate by far. |
| [`fr-store`](crates/fr-store) | In-memory data engine: `Value::{String,Hash,List,Set,SortedSet,Stream}`; `Entry` with TTL, LFU counter, modification count, encoding-promotion flags; positional `SCAN`/`HSCAN`/`SSCAN`/`ZSCAN` cursors; hash field TTL storage. |
| [`fr-expire`](crates/fr-expire) | `evaluate_expiry(now_ms, expires_at_ms)` with i64-clamp semantics, used by both lazy access paths and the active-expire cycle. |
| [`fr-persist`](crates/fr-persist) | AOF record codec + manifest, RDB v11 encoder/decoder with LZF and CRC64, standalone listpack decoder for upstream macro-node entries. |
| [`fr-repl`](crates/fr-repl) | Replication handshake FSM (`Init → PingSeen → AuthSeen → ReplconfSeen → PsyncSent → Online`), PSYNC decisioning, `ReplProgress` offset/ACK tracking, `WAIT`/`WAITAOF` evaluator. |
| [`fr-config`](crates/fr-config) | `Mode::{Strict,Hardened}`, `ThreatClass` taxonomy with decision actions, TLS configuration, encoding thresholds. |
| [`fr-runtime`](crates/fr-runtime) | The `Runtime` orchestrator: `ServerState`, `ClientSession`, ACL, transactions, threat-event ledger, AOF/replication signal capture, 230+ recognised `CONFIG` keys. |
| [`fr-eventloop`](crates/fr-eventloop) | Deterministic event-loop planning with per-phase tick budgets and phase-replay validators. |
| [`fr-server`](crates/fr-server) | The `frankenredis` binary; mio-based single-threaded TCP server; per-connection read/write buffers; blocked-client session swap; replica socket lifecycle. |
| [`fr-bench`](crates/fr-bench) | TCP benchmark harness for 8 workloads with HdrHistogram percentile reporting. |
| [`fr-conformance`](crates/fr-conformance) | Differential conformance harness + 13 binaries (live oracle diff, budget orchestrator, adversarial triage, schema gate, …). |
| [`fr-sentinel`](crates/fr-sentinel) | Sentinel reimplementation: discovery, health, consensus, failover, full `SENTINEL` command set. |

---

## How a command flows through the stack

Trace of a single `SET hello world` from the moment its bytes hit the socket until the `+OK\r\n` reply leaves it:

```
┌─ 1. Wire bytes arrive on a connection token in mio. ────────────────────────┐
│   *3\r\n$3\r\nSET\r\n$5\r\nhello\r\n$5\r\nworld\r\n                         │
│                                                                              │
│   fr-server reads them into ClientConnection::read_buf (a Vec<u8>            │
│   pre-allocated to ~4 KiB and grown as needed under output-buffer limits).   │
└──────────────────────────────────────────────────────────────────────────────┘
              │
              ▼
┌─ 2. RESP frame parse (fr-protocol::parse_frame_with_config). ───────────────┐
│   Walks the buffer respecting ParserConfig limits                            │
│   (max_bulk_len = 512 MiB, max_array_len = 1M, max_recursion_depth = 128).   │
│   Emits RespFrame::Array(vec![BulkString("SET"), BulkString("hello"),        │
│                                BulkString("world")]).                        │
│   On `Incomplete`, leave bytes in the buffer and wait for the next read.    │
└──────────────────────────────────────────────────────────────────────────────┘
              │
              ▼
┌─ 3. fr-runtime::Runtime::execute_frame. ────────────────────────────────────┐
│   - Frame → argv conversion (done once, passed down — no re-parsing).       │
│   - Preflight policy:                                                        │
│       · Strict/Hardened mode gate (fr-config::RuntimePolicy).                │
│       · ACL: is_command_allowed_for_argv(argv) — walks the user's            │
│         rule list with deny-first precedence + key-pattern checks.           │
│       · Subscription mode restriction (only SUBSCRIBE-family allowed).       │
│       · Transaction state: queue if in MULTI, dispatch if not.               │
│       · Maxmemory pressure (skip on reads; eviction tick on writes).         │
│   - Active-expire pulse: scan a bounded number of expiring keys, lazy-      │
│     expire what's due. Bounded by configurable budget.                       │
│   - Threat-event preflight: if input shape matches a threat class           │
│     (e.g. oversized bulk in hardened mode), record an event and             │
│     decide via DecisionAction.                                               │
└──────────────────────────────────────────────────────────────────────────────┘
              │
              ▼
┌─ 4. fr-command::dispatch_argv. ─────────────────────────────────────────────┐
│   Matches the verb against ~231 distinct command-name arms.                  │
│   For SET: parses tail options (EX/PX/EXAT/PXAT/KEEPTTL/NX/XX/GET),         │
│   validates argv arity, calls Store::set with computed expires_at_ms,       │
│   the value bytes, and the option flags.                                     │
└──────────────────────────────────────────────────────────────────────────────┘
              │
              ▼
┌─ 5. fr-store::Store::set. ──────────────────────────────────────────────────┐
│   - Encodes the per-database key (encode_db_key prefix).                    │
│   - Decides String encoding: int / embstr / raw based on payload shape      │
│     and on Entry::force_raw_encoding stickiness.                            │
│   - Constructs/updates Entry { value, expires_at_ms, last_access_ms,        │
│     lfu_freq, lfu_last_touch_min, modification_count++, encoding flags }.    │
│   - Bumps the dirty counter (visible in INFO Persistence).                  │
│   - Touches WATCH per-key modification counters (for ABA detection).        │
│   - If memory pressure, evicts per maxmemory-policy.                        │
└──────────────────────────────────────────────────────────────────────────────┘
              │
              ▼
┌─ 6. Persistence + replication signal capture (runtime). ────────────────────┐
│   - AOF: capture_aof_record(argv) — append the canonical RESP-encoded       │
│     command to the AOF buffer; flushed per appendfsync policy.              │
│   - Replication: advance the primary write offset; queue the bytes into     │
│     the BacklogWindow; mark connected replica sockets WRITABLE so the       │
│     event loop flushes the new backlog tail on the next tick.               │
│   - Keyspace notifications: if notify-keyspace-events is enabled and        │
│     covers strings, publish `__keyspace@0__:hello` / `__keyevent@0__:set`. │
└──────────────────────────────────────────────────────────────────────────────┘
              │
              ▼
┌─ 7. Reply encoding (fr-protocol::encode + fr-server write). ────────────────┐
│   The handler returns RespFrame::SimpleString("OK").                        │
│   The encoder writes "+OK\r\n" to ClientConnection::write_buf.              │
│   Write is coalesced per poll-cycle: all replies queued during this tick    │
│   go out in a single write() syscall (or as many as the kernel accepts);    │
│   anything not drained re-arms Interest::Writable for the next tick.        │
└──────────────────────────────────────────────────────────────────────────────┘
```

The whole path is single-threaded, which is a deliberate choice for determinism. The mio loop is `non-blocking I/O + one CPU core`, and that's where the deterministic-latency contract is actually enforced.

---

## Implementation deep-dives

Opt-in detail for engineers who want to understand the internals (algorithms, data layouts, invariants) rather than just operate the server.

### Data engine internals (`fr-store`)

Every key in FrankenRedis is a `Vec<u8>` mapping to an `Entry`:

```rust
struct Entry {
    value: Value,
    expires_at_ms: Option<u64>,
    last_access_ms: u64,         // OBJECT IDLETIME / LRU eviction
    lfu_freq: u8,                // OBJECT FREQ; LFU eviction
    lfu_last_touch_min: u64,     // logarithmic LFU decay
    modification_count: u64,     // WATCH ABA detection
    // One-way encoding-promotion sticky flags (mirror upstream
    // semantics — once promoted, never demoted).
    force_raw_encoding: bool,
    force_string_encoding: bool,
    force_set_listpack_encoding: bool,
    force_set_hashtable_encoding: bool,
    force_hash_hashtable_encoding: bool,
    force_zset_skiplist_encoding: bool,
}

enum Value {
    String(Vec<u8>),
    Hash(BTreeMap<Vec<u8>, Vec<u8>>),
    List(VecDeque<Vec<u8>>),
    Set(BTreeSet<Vec<u8>>),
    SortedSet(SortedSet),                  // dual-indexed
    Stream(StreamEntries),                 // BTreeMap<(ms, seq), Vec<(field, value)>>
}
```

**Why dual-index sorted sets?** Redis uses a hash table (member → score) and a skiplist (ordered by score, then member). FrankenRedis's `SortedSet` keeps the same two-sided contract (`O(1)` ZSCORE / ZADD lookup, `O(log n)` range scans) but realizes the ordered side with a `BTreeMap<ScoreMember, ()>` instead of a hand-rolled skiplist:

```rust
pub struct SortedSet {
    dict:    HashMap<Vec<u8>, f64>,           // member → score
    ordered: BTreeMap<ScoreMember, ()>,       // (score, member) → ()
}
```

`ScoreMember` wraps the `(f64, Vec<u8>)` pair to give Redis-compatible total ordering: `f64::total_cmp` for the score (handles NaN, -0 vs +0, +∞/-∞) with the lexicographic member as tie-breaker. Range queries by score (`ZRANGEBYSCORE`), by lex (`ZRANGEBYLEX`), and by rank (`ZRANGE 0 -1`) all become standard `BTreeMap::range` traversals. The result is the same algorithmic complexity as upstream with substantially less code.

**Encoding promotion is sticky.** Real Redis promotes encodings one-way: an `intset` that gets a non-integer member promotes to `listpack`, and once promoted it never demotes even if the offending member is removed. FrankenRedis enforces the same sticky contract via per-`Entry` boolean flags. The promotion paths are:

| Type | Initial | First promotion | Second promotion |
|---|---|---|---|
| String | `int` or `embstr` (≤44 B) | `raw` (any in-place mutation) | — |
| Hash | inline `BTreeMap` (treated as `listpack` by `OBJECT ENCODING`) | `hashtable` (exceeds `hash-max-listpack-{entries,value}`) | — |
| List | `VecDeque` (treated as `listpack`/`quicklist` by `OBJECT ENCODING`) | `quicklist` (exceeds `list-max-listpack-size`) | — |
| Set | `intset` (all-integer members) | `listpack` (first non-integer) | `hashtable` (exceeds `set-max-listpack-{entries,value}`) |
| SortedSet | `listpack` (small + short values) | `skiplist` (exceeds `zset-max-listpack-{entries,value}`) | — |

`OBJECT ENCODING` reports the post-promotion name, which matters because real-world clients sometimes branch on it (e.g., to know whether a `HGETALL` is `O(n)` or `O(1)` in the hash size).

**SCAN cursors are positional, not bit-reversed.** Real Redis uses a clever reverse-binary cursor that survives hash-table rehashing without missing or duplicating keys. FrankenRedis's `HashMap` doesn't have Redis's two-table rehash, so it can get away with a simpler positional cursor: `next_cursor = pos`, returning `0` on completion. Redis explicitly documents SCAN cursors as opaque to clients, so this is allowed; `HSCAN`/`SSCAN`/`ZSCAN` short-circuit small-encoding values (return everything with cursor = 0) just like upstream does.

**Hash field TTL** has its storage layer wired (`hash_field_expires: BTreeMap<(key, field), expires_at_ms>` plus per-key reap counters) and RDB round-trips through `RDB_TYPE_HASH_WITH_TTLS` (tag 100) already preserve per-field expirations across both runtimes. What is *not* yet wired is the wire-level command dispatch: the Redis 7.4 `HEXPIRE`/`HTTL`/`HPERSIST` family does not have command-arm entries in `fr-command::dispatch_argv` yet, and the lazy-expiry enforcement at every hash read path is still to come.

**LFU is the logarithmic counter from upstream** with `LFU_INIT_VAL = 5` (newly-created objects start at 5 so they aren't immediate eviction candidates) and `lfu-log-factor = 10` controlling counter growth speed; decay is applied via `lfu_last_touch_min` so the counter doesn't grow without bound during long-running cache sessions.

### Lua scripting internals (`fr-command::lua_eval`)

The Lua evaluator is **a clean-room Lua 5.1 interpreter written in safe Rust**: no `mlua`, no `rlua`, no embedded C Lua VM. The entire script runs against this value enum:

```rust
pub enum LuaValue {
    Nil,
    Bool(bool),
    Number(f64),
    Str(Vec<u8>),
    Table(LuaTable),
    Function(LuaFunc),
    RustFunction(String),     // name of a built-in
    Coroutine(LuaCoroutine),
    WrappedCoroutine(LuaCoroutine),
}

pub struct LuaTableInner {
    pub array: Vec<LuaValue>,                       // 1..#t sequence part
    pub string_hash: HashMap<Vec<u8>, LuaValue>,    // string-key fast path
    pub other_hash: Vec<(LuaValue, LuaValue)>,      // boolean/number/table keys
    pub other_keys: HashSet<LuaHashKey>,            // O(1) existence for non-string keys
    pub metatable: Option<LuaTable>,
}
```

A few design notes that matter when porting scripts:

- **Tables have three storage compartments.** The array part for `1..#t`, a string-hash for the overwhelmingly common case of `t.foo`, and a generic kv vector for boolean/number/table keys (with a `HashSet` index to keep existence checks `O(1)`). This is the same shape Lua 5.1 uses internally and is what `pairs`/`ipairs`/`next` walk.
- **Function values are legal table keys.** This is Lua-spec-compliant (a recently closed parity bead).
- **Metamethods that matter for Redis scripts are all implemented.** `__index`/`__newindex` (function or table), `__call`, `__concat`, the arithmetic family (`__add`/`__sub`/`__mul`/`__div`/`__mod`/`__pow`/`__unm`), `__eq`/`__lt`/`__le`, `__tostring`, and `__metatable` protection all match vendored Redis Lua 5.1 semantics including the dispatch ordering between the left and right operands. Lua 5.1 metamethods not exposed today (because Redis scripts essentially never use them under the upstream lua_time_limit budget): `__mode` (weak tables), `__len`, `__gc`.
- **The pattern matcher is full Lua 5.1.** All character classes, all quantifiers (`*`/`+`/`-`/`?`), anchors, captures, and sets, plus the trickier `%b()` balanced match, `%f[set]` frontier, and `%1`–`%9` back-references. The same engine drives `string.match`, `string.gmatch`, `string.gsub`, and `string.find`. (`gmatch` iterators are callable outside `for-in` loops, which matters when scripts pass them as closures.)
- **`cjson` is the bundled upstream encoder/decoder.** `cjson.encode` formats numbers with `%.14g` (the upstream printf format) and escapes `/` exactly the way Redis does. `cjson.decode` is currently permissive about trailing commas and non-string keys; full strict-mode rejection is tracked as a parity bead.
- **The `redis.*` namespace** carries `redis.call`, `redis.pcall`, `redis.error_reply`, `redis.status_reply`, `redis.sha1hex`, `redis.log`, `redis.replicate_commands`, `redis.set_repl`, `redis.setresp`, `redis.acl_check_cmd`, `redis.breakpoint`, `redis.debug`, plus the `redis.REPL_NONE`/`REPL_AOF`/`REPL_SLAVE`/`REPL_REPLICA`/`REPL_ALL` constants (script-side aliases for the upstream `PROPAGATE_*` C enum) and `redis.LOG_DEBUG`/`LOG_VERBOSE`/`LOG_NOTICE`/`LOG_WARNING` log-level constants. `redis.call` propagates the calling script's context so nested calls keep the same replication / RESP version / ACL identity.
- **Pcall shape is precisely upstream.** When a built-in raises, the message is packaged inside `pcall` exactly the way Redis's vendored Lua does: anonymous template for C-builtin errors, named template for Lua errors, `ERR ` prefix where upstream uses one, callable line tracking for `loadstring` chunks. Most of Phase 11 was iterative closure of this surface, one wording at a time.
- **Coroutines work.** `coroutine.create`/`resume`/`yield`/`wrap` are all live; `WrappedCoroutine` exists so `coroutine.wrap` can return a closure that resumes a hidden coroutine without exposing it.
- **Closures with upvalue capture work.** Lexical scoping is real: `local x = 5; local f = function() return x end` captures `x` by upvalue, not by re-evaluation.
- **Known sandbox gaps:** `cmsgpack`, `struct`, `setfenv`/`getfenv`, and `newproxy` are not exposed. Each is a tracked parity bead.

The evaluator is exercised by:
- A dedicated `fuzz_lua_eval` cargo-fuzz target with checked-in seed corpus.
- 272 `core_scripting` conformance fixtures (the fifth-largest fixture family, behind `core_zset` 324, `core_strings` 307, `core_server` 282, and `core_stream` 273).
- A separate `fuzz_function_restore` target covering `FUNCTION DUMP`/`RESTORE` round-trips through the RDB envelope.

### Event loop discipline: DLRC in practice (`fr-eventloop`)

The "Deterministic Latency" half of DLRC is realized by an event loop that is *planned* rather than just *run*. Each tick goes through five named phases in fixed order:

```rust
pub enum EventLoopPhase {
    BeforeSleep,    // active-expire, AOF flush per appendfsync everysec, ...
    Poll,           // epoll_wait / mio Poll::poll with computed timeout
    FileDispatch,   // process readable / writable connection events
    TimeDispatch,   // process timers (blocked-client deadlines, replication ACKs)
    AfterSleep,     // post-poll bookkeeping
}

pub const EVENT_LOOP_PHASE_ORDER: [EventLoopPhase; 5] = [/* the five above */];
```

A `TickBudget` carries `max_accepts` and `max_commands` per tick so neither a thundering-herd accept storm nor a single hyperactive client can starve the rest of the loop. Each phase has *planning* code and *execution* code; the planning side emits validators that any agent or test can run against the recorded phase trace to confirm the loop did exactly what it said it would. The "replay/audit can reconstruct what the loop did each tick" property is what those validators actually enforce.

Blocking commands (BLPOP/BRPOP/BLMOVE/BLMPOP/BZPOPMIN/BZPOPMAX/BZMPOP/XREAD BLOCK/XREADGROUP BLOCK/WAIT/WAITAOF/CLIENT PAUSE) plug into this discipline via the `BlockedState { op, deadline_ms }` field on each `ClientConnection`. When the source key gets data (or the deadline arrives in `TimeDispatch`), the command is re-dispatched with the fresh state. Nothing actually blocks an OS thread; the whole server stays inside its single mio loop.

### Threat-event ledger format

Every time the hardened-mode policy engine sees something unusual, the runtime calls `Runtime::record_threat_event(...)` with the inputs needed to materialize a deterministic ledger row. The recorded row carries (at minimum):

```
ThreatEvent
├── now_ms              u64                    server wall-clock at decision time
├── packet_id           u64                    monotonic per-runtime sequence number
├── subsystem           &'static str           "protocol" / "persistence" / "replication" / …
├── action              &'static str           "reject" / "clamp" / "downgrade" / …
├── reason_code         &'static str           short machine-readable tag
├── reason              String                 one-line human-readable narrative
├── threat_class        ThreatClass            one of 8 (see table below)
├── preferred_deviation Option<HardenedDeviationCategory>
├── input_digest        [u8; 32]               SHA256 of the offending input bytes
├── output              RespFrame              what the server returned to the client
└── decision_action     DecisionAction         FailClosed | BoundedDefense | RejectNonAllowlisted
```

The matching two-round optimization that recovered the April 7 throughput gap is documented in `artifacts/optimization/throughput-gap/ISOMORPHISM_PROOF_LAZY_DIGEST.md`. Round 1 makes `Store::state_digest`, `input_digest`, and `state_digest_before/after` lazy (gated behind `self.policy.emit_evidence_ledger`) so the success path pays zero digest cost; the eager precomputation that previously ran on every command was discarded by the threat-event ledger 99.99% of the time. Round 2 short-circuits ACL category resolution for users whose `denied_categories` and `allowed_categories` are both empty (the default `+@all` user), skipping a per-command scan of every ACL category against the full command table (~5,500 string-splitting iterations per command today, with 23 categories × ~240 commands). Together they moved single-command throughput from ~1.3% to 79-99% of Redis. See the Phase 9 CHANGELOG entry for the wider recovery story.

The eight threat classes are:

| Class | Examples |
|---|---|
| `ParserAbuse` | RESP framing larger than allowed, recursion bombs, CRLF injection in error bodies |
| `MetadataAmbiguity` | RDB AUX fields disagreeing with payload, AOF tail with mid-record CRLF |
| `VersionSkew` | Inbound RESP3 type that downgrades unsafely, RDB version older than supported |
| `ResourceExhaustion` | Bulk size approaching the per-config ceiling, unbounded SCAN MATCH on huge keyspaces |
| `PersistenceTampering` | RDB CRC64 mismatch, truncated AOF chunk |
| `ReplicationOrderAttack` | Out-of-order replication offset, replica reconnect with mismatched run-id |
| `AuthPolicyConfusion` | ACL deny-allow conflict, deprecated user attempting privileged op |
| `ConfigDowngradeAbuse` | Attempt to widen a protected CONFIG without proper authorization |

Decision actions:

- **`FailClosed`**: reply with an error, log the event, do not mutate state.
- **`BoundedDefense`**: apply a contract-preserving repair (e.g., clamp an oversized bulk to the ceiling), log the event, continue.
- **`RejectNonAllowlisted`**: reply normally if the threat class is in the operator's `HARDENED_ALLOWLIST_DEFAULT`, otherwise treat as FailClosed.

In strict mode every threat class defaults to `FailClosed` with `DriftSeverity::S0`; strict mode never silently defends.

### RDB v11 byte layout, walked

A FrankenRedis RDB file from `BGSAVE` looks like this on the wire (annotated to match vendored byte-for-byte):

```
+----------------------------------------------------+
| "REDIS" (5 bytes)                                  |
| "0011"  (4 bytes — RDB version 11)                 |
+----------------------------------------------------+
| AUX header opcodes (0xFA <key-string> <val-string>)|
|   "redis-ver"   → "7.2.4"   (REDIS_COMPAT_VERSION) |
|   "frankenredis"→ "true"    (origin marker)        |
|   (Other AUX fields like "redis-bits", "ctime",    |
|    "used-mem", "aof-base" are also *accepted* on   |
|    decode when produced by vendored Redis but are  |
|    not emitted by FrankenRedis BGSAVE today.)      |
+----------------------------------------------------+
| For each non-empty database:                       |
|   SELECTDB     (0xFE) <db-index>                   |
|   RESIZEDB     (0xFB) <key-count> <expire-count>   |
|   For each key in this db:                         |
|     [EXPIRETIME_MS (0xFC) <8 bytes little-end>]    | optional
|     <RDB_TYPE_*>  (one byte, see table below)      |
|     <key>         (RDB string-length encoded)      |
|     <value>       (per-type, see below)            |
+----------------------------------------------------+
| EOF (0xFF)                                         |
| CRC64 footer (8 bytes, little-endian, Redis poly)  |
+----------------------------------------------------+
```

Type tags currently understood:

| Tag | Name | Meaning |
|---|---|---|
| 0  | `RDB_TYPE_STRING`           | Raw / embstr / int (encoded via length) |
| 1  | `RDB_TYPE_LIST`             | Legacy linked list |
| 2  | `RDB_TYPE_SET`              | Hash-table set |
| 4  | `RDB_TYPE_HASH`             | Plain hash table |
| 5  | `RDB_TYPE_ZSET_2`           | Sorted set with binary LE double scores |
| 11 | `RDB_TYPE_SET_INTSET`       | Intset (all-integer set) |
| 15 | `RDB_TYPE_STREAM_LISTPACKS`   | Stream — FrankenRedis encoding |
| 16 | `RDB_TYPE_HASH_LISTPACK`    | Hash in listpack form |
| 17 | `RDB_TYPE_ZSET_LISTPACK`    | Sorted set in listpack form |
| 18 | `RDB_TYPE_LIST_QUICKLIST_2` | Quicklist of listpacks |
| 19 | `RDB_TYPE_STREAM_LISTPACKS_2` | Stream — upstream 6.2+ |
| 20 | `RDB_TYPE_SET_LISTPACK`     | Set in listpack form |
| 21 | `RDB_TYPE_STREAM_LISTPACKS_3` | Stream — upstream 7.4+ |
| 100| `RDB_TYPE_HASH_WITH_TTLS`   | Hash with per-field TTLs (Redis 7.4 / fr) |

**LZF compression.** Strings >20 B are LZF-compressed using a pure-Rust port of Marc Lehmann's algorithm. The wire format is the exact compressed-byte layout vendored emits, verified by round-trip fuzzing through both servers (`fuzz_rdb_encode_round_trip` and the live RDB corpus harvester).

**CRC64 footer.** The hash is computed over every byte of the file from the `"REDIS"` magic through the EOF opcode, using the same `CRC64_REDIS_POLY` polynomial vendored uses. A trailing mismatched CRC fails closed in both strict and hardened mode (it's `PersistenceTampering`).

**FUNCTION DUMP envelope.** `FUNCTION DUMP <lib>` returns a payload that begins with the RDB version, contains the function body as an RDB-string-encoded blob, and ends with a CRC64 footer. That is the same envelope vendored uses, so `FUNCTION RESTORE` works across both servers in either direction.

### Replication: offset arithmetic, FSM, and the backlog window

Replication is governed by three small invariants:

```
primary_offset      = total bytes of write-stream emitted so far
replica_ack_offset  = bytes the replica has confirmed receiving
lag_bytes           = primary_offset - replica_ack_offset    (saturating)
```

Offset arithmetic is saturating in both directions, so an ACK that briefly arrives ahead of the primary's local counter (clock drift, retransmit) cannot wrap a `u64`. `WAIT n_replicas timeout_ms` blocks the calling client until at least `n` replicas have an `ack_offset >= primary_offset_at_call_time` or the deadline fires. `WAITAOF` is the same shape but counts replicas whose **fsync'd** offset has caught up, which matters when the operator wants "the data is durable on the local AOF AND on the replica's AOF" semantics.

The handshake FSM on the replica side is:

```
   ┌───────┐  send PING        ┌────────────┐
   │ Init  │ ──────────────────│ PingSeen   │
   └───────┘                   └─────┬──────┘
                                     │  primary needs AUTH?
                                     ▼
                              ┌────────────┐
                              │ AuthSeen   │  (otherwise skipped)
                              └─────┬──────┘
                                    │ send REPLCONF listening-port, capa eof, capa psync2
                                    ▼
                            ┌──────────────┐
                            │ ReplconfSeen │
                            └─────┬────────┘
                                  │ send PSYNC <repl-id> <offset>
                                  ▼
                          ┌────────────────┐
                          │ PsyncSent      │
                          └─────┬──────────┘
                                │ decide_psync():
                                │   - FULLRESYNC <new-id> <offset>  → stream RDB
                                │   - CONTINUE <maybe-new-id>       → backlog replay
                                ▼
                          ┌─────────────┐
                          │ Online      │ ── REPLCONF ACK every ~1s ─┐
                          └─────┬───────┘                            │
                                │                                    │
                                └─────── socket drops? ──────────────┘
                                          reconnect with backoff,
                                          restart from Init.
```

The primary keeps a `BacklogWindow` (a ring of recent write-stream bytes) so that a CONTINUE can serve a reconnecting replica without falling back to FULLRESYNC. `repl-backlog-size` is hot-reloadable via `CONFIG SET`; the window is recomputed without dropping connected replicas.

Replica chaining (replica-of-a-replica) works because the intermediate replica forwards its own primary-write-stream-bytes downstream; the downstream sees a normal primary, just with a different run-id. Integration tests exercise both `legacy-redis-primary → fr-replica` and `fr-primary → fr-replica → fr-downstream-replica` end-to-end.

### PSYNC continuation: a worked numeric example

Suppose a primary has been running for a while and the backlog window holds bytes from primary offset `300_000` (oldest) through `1_000_000` (newest). A replica disconnects briefly and reconnects:

```
Replica state at reconnect:
  cached_master_repl_id = "5c4b3a..."
  last_acked_offset     = 950_000

Replica sends:
  PSYNC 5c4b3a... 950000

Primary's decide_psync runs two checks (fr-repl::decide_psync):
  1. requested_replid.eq_ignore_ascii_case(backlog.replid)  →  YES
  2. backlog.contains(requested_offset = 950000)             →  YES
     (the backlog window is [300000, 1000000], so 950000 is in range)

Primary responds:
  +CONTINUE <replid>
  (no new repl-id needed because the existing one still matches)

Primary then streams the 50_000 missing bytes from offset 950000 to 1000000
and resumes normal write propagation. No RDB snapshot is sent.
```

If any check fails (replica had stale repl-id, or its offset is older than the backlog's oldest byte, or the gap exceeds the configured `repl-backlog-size`), the primary falls back to:

```
+FULLRESYNC <new-repl-id> 1000000
<RDB snapshot stream>
<resume normal propagation>
```

The CONTINUE path is essentially free; the FULLRESYNC path requires forking a child to snapshot the keyspace and streaming the whole dataset. `repl-backlog-size` exists to let operators trade memory for the probability that any given disconnect can recover via CONTINUE. As a rule of thumb, set it to `peak write throughput × tolerable disconnect window`.

---

## Algorithm deep-dives

Redis is a collection of small, well-chosen algorithms, each solving one observable behavior efficiently. FrankenRedis re-implements all of them in safe Rust. The notable ones:

### HyperLogLog (PFADD / PFCOUNT / PFMERGE)

PFADD/PFCOUNT estimate the cardinality of a multiset in 12 KiB of memory with ~0.81% standard error. The math is the Flajolet/Fusy/Gandouet/Meunier 2007 HyperLogLog algorithm, with the Heule et al. 2013 "HyperLogLog in Practice" small-cardinality and bias corrections.

```
For each element x added via PFADD:
  h = murmur3-style 64-bit hash of x
  i = high 14 bits of h        (selects one of 2^14 = 16384 registers)
  w = remaining 50 bits of h
  M[i] = max( M[i], 1 + position_of_first_1_bit_in(w) )

PFCOUNT then estimates cardinality from the 16384 registers using the
HLL harmonic-mean formula plus the small/large cardinality bias corrections.
```

FrankenRedis ships the **dense** representation (`16389 bytes = 16384 6-bit registers packed into 12 KiB plus a 5-byte HLL_DENSE header`). Vendored Redis uses a sparse representation for low cardinalities and switches to dense when it gets bigger; that's the open `frankenredis-j2tuo` parity bead. Switching to sparse for small sets saves a lot of memory when you have many keys each holding a small HLL.

`PFMERGE` produces a union by taking the register-wise max across the inputs. `PFDEBUG GETREG/DECODE/ENCODING/TODENSE` inspect or convert the encoding. `PFSELFTEST` is the upstream-defined self-check covering register bit-packing and hash-distribution invariants.

### Geo (GEOADD / GEOSEARCH / …)

Geo data is stored *inside a sorted set*; there is no separate geo type. The score is a 52-bit interleaved geohash, computed by quantizing latitude and longitude into bit streams and interleaving them:

```
For each (lon, lat) pair:
  lon_bits = quantize(lon, -180..+180, 26 bits)
  lat_bits = quantize(lat, -85.05..+85.05, 26 bits)
  hash    = interleave_bits(lon_bits, lat_bits)   // 52 total bits
  ZADD key hash member
```

This gives two useful properties: (1) **nearby points have similar scores**, so `ZRANGEBYSCORE` over a tight score range is approximately a bounding-box query in geo space; (2) the inverse, `decode_geohash(score) → (lon, lat)`, is exact within the 26-bit quantization.

`GEOSEARCH FROMLONLAT lon lat BYRADIUS r m` works by:

1. Computing a coarse outer bounding geohash range that's guaranteed to contain every point within `r` meters of `(lon, lat)`.
2. Pulling all members whose score is in that range (a `ZRANGEBYSCORE`).
3. For each candidate, computing the **haversine** great-circle distance and discarding those farther than `r`.

`GEODIST` is just step 3 between two members. The whole module is a few hundred lines of math on top of `fr-store`'s ZSet, with no separate index.

### BITFIELD: atomic multi-counter ops on a single string

`BITFIELD key GET|SET|INCRBY|OVERFLOW type offset [value]` lets you treat a single Redis string as a packed array of fixed-width signed (`i#`) or unsigned (`u#`) integers, and do **multiple operations atomically** in one call:

```
BITFIELD counter SET u8 0 42 INCRBY i16 #1 -3 OVERFLOW SAT INCRBY u4 #2 15
```

- `u8 0`: 8-bit unsigned int starting at byte 0
- `i16 #1`: signed 16-bit int starting at offset `1 * 16 = 16` bits (the `#` prefix multiplies by the type width)
- `u4 #2`: 4-bit unsigned int starting at offset `2 * 4 = 8` bits
- `OVERFLOW {WRAP|SAT|FAIL}` selects what happens on overflow per Redis spec: WRAP (silent two's-complement wrap), SAT (saturate at type min/max), FAIL (return nil)

FrankenRedis enforces the upstream **4 GiB bit-offset ceiling** on every operation (a parity fix from the April–May sweep) and clamps overflow per the active `OVERFLOW` mode. The single-call atomicity is real because the operations run inside one synchronous `BITFIELD` handler, so no other client can observe an intermediate state.

### Active-expire pulse (the lazy/active hybrid)

Redis uses a two-pronged TTL strategy: **lazy** expiry (any access to an expired key drops it before returning the not-found reply) plus an **active** background cycle that periodically scans expiring keys. FrankenRedis ships both:

```
fn run_active_expire_cycle(now_ms, start_cursor, sample_limit):
  // Pull `sample_limit` keys starting from the cursor position in
  // the sorted `ordered_keys` index (wrapping around at the end).
  keys = ordered_keys.range(start_cursor..).take(sample_limit).chain(...wraparound...)

  evicted = 0
  for key in keys:
    if evaluate_expiry(now_ms, entry.expires_at_ms).should_evict:
      emit_keyspace_event("expired", key)
      remove(key)
      evicted += 1

  return ActiveExpireCycleResult { sampled, evicted, next_cursor }
```

This is a **deterministic cursor-based scan** rather than the random-sampling-with-25%-recurrence heuristic upstream uses. Every expiring key is eventually visited as the cursor walks the keyspace; the budget bound (`sample_limit`) prevents any one tick from monopolizing the loop. The trade-off versus upstream is more predictable progress and simpler reasoning, at slightly higher cost on workloads where most expirations cluster temporally. The cursor is persisted across ticks so the scan is fair across the keyspace, not biased toward whatever is at the front of the index.

### LRU / LFU eviction (note: exact, not approximate)

`maxmemory-policy {allkeys-lru, volatile-lru, allkeys-lfu, volatile-lfu, allkeys-random, volatile-random, volatile-ttl, noeviction}` triggers when adding a key would exceed `maxmemory`. Two pieces of FrankenRedis behavior are worth calling out explicitly because they differ from vendored Redis:

1. **Eviction selection is currently an exact scan.** `select_eviction_candidate` walks every `Entry` in `Store::entries` to find the one with the smallest `last_access_ms` (for LRU policies) or the soonest `expires_at_ms` (for `volatile-ttl`). Upstream Redis approximates this with `maxmemory-samples` random samples plus an `EVPOOL_SIZE = 16` candidate pool. FrankenRedis recognizes `maxmemory-samples` in `CONFIG SET` for compatibility but does not currently use it for selection; selection is exact and `O(N)` per eviction.
2. **LFU is currently approximated as LRU at selection time.** The 8-bit LFU counter is tracked per-`Entry` and exposed via `OBJECT FREQ` (using the standard upstream logarithmic-increment + minute-resolution-decay arithmetic), but `select_eviction_candidate` for `allkeys-lfu` / `volatile-lfu` falls back to picking the key with the smallest `last_access_ms` rather than the smallest `lfu_freq`. The counter is correct; the selection isn't yet wired to it.

Both are worth tracking. The exact scan is straightforward to switch to sampling, and the LFU-selection wiring is a small change. Neither affects the observable contract of `OBJECT IDLETIME` / `OBJECT FREQ` or the `evicted_keys` counter, but they do affect throughput at very large keyspaces and the *which key gets evicted* decision under LFU.

The LFU counter itself follows upstream `evict.c`: logarithmic increment so a hot key doesn't saturate the 8-bit counter to 255 immediately (it grows roughly like `log_2(access_count)` modulated by `lfu-log-factor`), plus a separate decay (`lfu-decay-time` minutes) so a once-hot-then-cold key doesn't stick forever.

### Glob matching (KEYS, SCAN MATCH, PSUBSCRIBE, ACL `~pattern`, …)

The same `glob_match` engine in `fr-store` powers every place Redis accepts a pattern:

```
*    matches zero or more bytes
?    matches exactly one byte
[ab] matches one byte in the class
[^ab] matches one byte NOT in the class
[a-z] character ranges inside a class
\x   escapes the next character literally
```

It is implemented as a recursive descent matcher with a single-byte lookahead, the same shape as upstream `stringmatchlen()`, chosen because it is straightforward to fuzz to byte-for-byte parity. The `fuzz_glob_match` target keeps it honest against vendored.

### Listpack encoding format

Small hashes, sets, sorted sets, and stream entries are stored in **listpack** form to amortize allocation and cache cost. The on-disk byte layout (what `fr-persist` reads/writes) is:

```
+--------------------------+
| total_bytes (4 bytes)    |  little-endian
| num_elements (2 bytes)   |  little-endian (0xFFFF = "use forward scan")
+--------------------------+
| entry 0                  |
| entry 1                  |
| ...                      |
+--------------------------+
| end_marker (0xFF)        |
+--------------------------+

Each entry:
  encoding byte (1)         high bits select one of:
                              7-bit unsigned int (1 byte total)
                              6-bit length string (1+N bytes)
                              13-bit signed int
                              12-bit length string
                              16/24/32/64-bit signed int
                              16-bit length string
                              32-bit length string
  payload (variable)
  back-pointer (variable)   length encoded in 1-5 bytes; encodes the
                            position to walk backward in O(1)
```

The back-pointer makes listpack a **bidirectional** container; `LRANGE end -1` is as cheap to start from as `LRANGE 0 1`. FrankenRedis's standalone listpack decoder lives in `crates/fr-persist/src/listpack.rs` and is reused by both the RDB decoder (for upstream macro-node entries) and the stream entry parser.

### Pub/Sub fan-out cost model

Channel subscribers are tracked in two hash maps on `ServerState`:

```rust
pubsub_channel_subs: HashMap<Vec<u8>, HashSet<ClientId>>,   // channel → subscribers
pubsub_pattern_subs: HashMap<Vec<u8>, HashSet<ClientId>>,   // glob pattern → subscribers
pubsub_shard_subs:   HashMap<Vec<u8>, HashSet<ClientId>>,   // shard channel → subscribers
```

Cost of `PUBLISH channel msg`:
- Channel lookup: `O(1)`, direct hash hit.
- Per-pattern check: `O(num_patterns × glob_match_cost)`; every registered pattern is tested.
- Total: `O(num_channel_subs + num_pattern_subs)`; every subscriber gets the message appended to a per-client `pubsub_outbox` (a `HashMap<ClientId, Vec<PubSubMessage>>` on `ServerState`) in the same poll tick.

Cross-client delivery walks the subscriber set, pushes one `PubSubMessage::{Message,PMessage,SMessage}` envelope per subscriber into the outbox, and the event loop drains the outbox into each client's `write_buf` (encoding it as `*3\r\n$7\r\nmessage\r\n$<n>\r\n<channel>\r\n$<m>\r\n<payload>\r\n`) before the next `FileDispatch` phase flushes the buffers. There's no inter-thread queue because there's no second thread.

### Why a single-threaded mio loop (and not `tokio` / a thread pool)?

Operators ask about this choice often. Three reasons:

1. **Determinism.** Redis's behavior model is "one logical thread, commands execute atomically in arrival order." Multi-threading the data path means giving up that model; Dragonfly and KeyDB both had to invent per-shard sharding or per-connection serialization to recover something like it. FrankenRedis preserves the original semantics by inheriting the original design.
2. **Latency predictability.** A single-threaded mio loop has no lock contention, no context-switch jitter, no false-sharing across cores. The sub-millisecond p50 latency numbers in the Performance section fall out of that directly.
3. **Cognitive load.** The whole runtime fits in one synchronous mental model: read bytes, parse, dispatch, mutate, encode, write. There is no `tokio::spawn` to reason about, no `Arc<Mutex<...>>` over the store, no `async` color anywhere in the data path. The 4,975 conformance fixtures and 33 fuzz targets are all easier to write because of this.

The trade-off is that a single-threaded server doesn't scale across cores for raw throughput. Neither does vendored Redis, and the established operational pattern (one Redis process per core, partitioned by application sharding or by cluster) applies unchanged to FrankenRedis.

### Wire trace: `SET hello world` byte for byte

The literal hex of a `SET hello world` request and reply, for grounding:

```
Request                                          (35 bytes on the wire)
00000000  2a 33 0d 0a 24 33 0d 0a  53 45 54 0d 0a 24 35 0d   |*3..$3..SET..$5.|
00000010  0a 68 65 6c 6c 6f 0d 0a  24 35 0d 0a 77 6f 72 6c   |.hello..$5..worl|
00000020  64 0d 0a                                            |d..|

Reply                                              (5 bytes on the wire)
00000000  2b 4f 4b 0d 0a                                      |+OK..|
```

Reading the request:
- `*3\r\n` — RESP array header, 3 elements
- `$3\r\nSET\r\n` — bulk string of length 3 with payload `SET`
- `$5\r\nhello\r\n` — bulk string of length 5 with payload `hello`
- `$5\r\nworld\r\n` — bulk string of length 5 with payload `world`

Reading the reply: `+OK\r\n` is a RESP simple string carrying `OK`.

Every fixture in `fr-conformance` is some variation of this: argv in, expected reply out, then the harness checks the wire bytes against vendored Redis.

---

## Installation

### Prerequisites

- **Rust nightly** (the workspace uses Rust 2024 edition; the pin lives in `rust-toolchain.toml`).
- **A C toolchain** if you want to also build the vendored Redis under `legacy_redis_code/redis/` for differential testing.
- **`redis-server`** on your `PATH` if you want to run live differential conformance gates without using the vendored copy.

### From source

```bash
git clone https://github.com/Dicklesworthstone/frankenredis
cd frankenredis

# Single binary, release profile (LTO + codegen-units=1)
cargo build --release -p fr-server

./target/release/frankenredis --help
```

The workspace `Cargo.toml` does not currently override `[profile.release]`, so the release build uses cargo's defaults (`opt-level = 3`, `lto = false`, `codegen-units = 16`, `strip = "none"`). The doctrine in `AGENTS.md` describes a tuned profile (`lto = true`, `codegen-units = 1`, `strip = true`) that has not yet been added to the workspace root; opt in locally via `RUSTFLAGS` or a Cargo profile override if you need it.

### Build the bench harness

```bash
cargo build --release -p fr-bench
./target/release/fr-bench --help
```

### Build the conformance harness

```bash
cargo build --release -p fr-conformance --bin live_oracle_diff
./target/release/live_oracle_diff --help
```

### Workspace check / lint / test

```bash
cargo fmt --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test  --workspace
```

If you have `rch` (remote compilation helper) installed, all of the above can be offloaded transparently; see `AGENTS.md` for the multi-agent build conventions.

---

## Quick start

### Standalone server

```bash
./target/release/frankenredis \
  --bind 127.0.0.1 \
  --port 6379 \
  --mode strict \
  --aof ./data/appendonly.aof \
  --rdb ./data/dump.rdb
```

### Primary + replica + sentinel

```bash
# Primary
./target/release/frankenredis --port 6379 --aof p.aof --rdb p.rdb &

# Replica (connects to primary; FULLRESYNC then CONTINUE)
./target/release/frankenredis --port 6380 --aof r.aof --rdb r.rdb \
  --replicaof 127.0.0.1 6379 &

# (The fr-sentinel state machine and command surface are available; the
#  dedicated launcher binary that puts the server into sentinel mode at
#  startup is still to come. See the Sentinel section.)
```

### With auth

```bash
# Primary requires password
./target/release/frankenredis --port 6379 --config primary.conf

# Replica authenticates upstream
./target/release/frankenredis --port 6380 \
  --replicaof 127.0.0.1 6379 \
  --masterauth somepassword
```

### Talk to it

Any client that speaks RESP2 or RESP3 works: `redis-cli`, `iredis`, `hiredis`, `lettuce`, `node-redis`, `go-redis`, `redis-py`, etc.

```bash
redis-cli -p 6379
redis-cli -p 6379 --pipe < commands.txt
redis-cli -p 6379 monitor      # streaming MONITOR
redis-cli -p 6379 latency doctor
redis-cli -p 6379 acl whoami
```

---

## Configuration

FrankenRedis reads `--config <path>` in standard `redis.conf` format. The exact same keys you'd use in vendored Redis 7.2.4 are recognised. CLI flags override config-file values.

A minimal annotated config:

```conf
# ---- Network ----
bind            127.0.0.1
port            6379
tcp-backlog     511

# ---- Auth + ACL ----
# (Compatibility mode itself is a CLI flag: `--mode strict|hardened`,
#  defaulting to strict — no config-file key for it.)
requirepass     "change-me"
# Or load a checked-in users file:
# aclfile       /etc/frankenredis/users.acl

# ---- Persistence ----
appendonly      yes
appendfsync     everysec        # always | everysec | no
dir             /var/lib/frankenredis
dbfilename      dump.rdb

# Background snapshot triggers (Redis style: <seconds> <changes>)
save            3600 1
save            300 100
save            60  10000

# ---- Memory ----
maxmemory       4gb
maxmemory-policy allkeys-lru     # noeviction | allkeys-lru | allkeys-lfu |
                                  # allkeys-random | volatile-lru | volatile-lfu |
                                  # volatile-ttl  | volatile-random
maxmemory-samples 5

# ---- Replication ----
replica-read-only yes
repl-backlog-size  1mb
repl-timeout       60
repl-ping-replica-period 10
min-replicas-to-write 0
min-replicas-max-lag  10
# replicaof 10.0.0.1 6379
# masterauth "primary-password"

# ---- Lua scripting ----
lua-time-limit 5000
busy-reply-threshold 5000

# ---- Encoding thresholds (mirror upstream Redis 7.2.4 defaults) ----
hash-max-listpack-entries 512
hash-max-listpack-value    64
set-max-intset-entries    512
set-max-listpack-entries  128
set-max-listpack-value     64
zset-max-listpack-entries 128
zset-max-listpack-value    64
list-max-listpack-size     -2

# ---- Keyspace notifications ----
notify-keyspace-events ""

# ---- Slowlog / Latency ----
slowlog-log-slower-than 10000
slowlog-max-len         128
latency-monitor-threshold 0

# ---- Debug surface (default-deny per Redis 7.2) ----
enable-debug-command no
```

`CONFIG GET '*'` returns 230+ live keys (the static-defaults table in `fr-runtime` carries 232 entries; some are aliases for upstream spellings). `CONFIG REWRITE` writes back to the file passed via `--config`. Slave/replica aliases are emitted on both spellings (e.g. `slave-read-only` and `replica-read-only`).

### CLI flags

```
frankenredis [options]

  --port <PORT>                       Listen port (default: 6379)
  --bind <ADDR>                       Bind address (default: 127.0.0.1)
  --mode {strict|hardened}            Compatibility/security policy mode (default: strict)
  --aof <PATH>                        AOF file (enables AOF)
  --rdb <PATH>                        RDB snapshot file
  --config <PATH>                     redis.conf-compatible config file
  --replicaof <HOST> <PORT>           Become a replica of HOST:PORT
  --masteruser <USER>                 Username for upstream AUTH
  --masterauth <PASS>                 Password for upstream AUTH
  --enable-debug-command {no|local|yes}
                                      Allow DEBUG command surface (default: no;
                                      `local` permits DEBUG only over loopback)
  --help, -h                          Show help
```

---

## Command surface

All 241 base Redis commands are implemented and exposed. Counts below are approximate command-name groupings.

| Family | Count | Highlights |
|---|---|---|
| Strings | 22 | GET, SET (`EX`/`PX`/`EXAT`/`PXAT`/`KEEPTTL`/`NX`/`XX`/`GET`), SETEX, PSETEX, SETNX, APPEND, GETRANGE, SETRANGE, INCR, DECR, INCRBY, DECRBY, INCRBYFLOAT, GETEX, GETDEL, GETSET, MSET, MSETNX, MGET, LCS, SUBSTR, STRLEN |
| Hashes | 16 | HSET, HGET, HMGET, HDEL, HINCRBY, HINCRBYFLOAT, HSCAN, HRANDFIELD (`COUNT`/`WITHVALUES`), HSETNX, HVALS, HKEYS, HLEN, HEXISTS |
| Lists | 22 | LPUSH, RPUSH, LPOP/RPOP (`COUNT`), LRANGE, LTRIM, LINSERT, LPOS (`RANK`/`COUNT`/`MAXLEN`), LMOVE, RPOPLPUSH, LMPOP, BLPOP, BRPOP, BLMOVE, BLMPOP, BRPOPLPUSH |
| Sets | 17 | SADD, SREM, SISMEMBER, SMISMEMBER, SCARD, SMEMBERS, SINTER, SUNION, SDIFF, SINTERSTORE, SUNIONSTORE, SDIFFSTORE, SINTERCARD, SRANDMEMBER (`COUNT`), SPOP (`COUNT`), SSCAN, SMOVE |
| Sorted sets | 35 | ZADD (`NX`/`XX`/`GT`/`LT`/`CH`/`INCR`), ZRANGE (`BYSCORE`/`BYLEX`/`REV`/`LIMIT`/`WITHSCORES`), ZREVRANGE, ZRANGESTORE, ZRANGEBYSCORE/ZREVRANGEBYSCORE, ZRANGEBYLEX/ZREVRANGEBYLEX, ZPOPMIN/ZPOPMAX (`COUNT`), BZPOPMIN, BZPOPMAX, ZMPOP, BZMPOP, ZDIFF/ZUNION/ZINTER + STORE forms, ZINTERCARD, ZRANDMEMBER, ZINCRBY, ZRANK/ZREVRANK (`WITHSCORE`), ZSCORE, ZMSCORE, ZCOUNT, ZLEXCOUNT, ZCARD, ZREM, ZREMRANGEBYRANK/SCORE/LEX, ZSCAN |
| Streams | 15 | XADD, XLEN, XDEL, XTRIM (`MAXLEN`/`MINID`, `~`/`=`), XRANGE/XREVRANGE/XPENDING/XAUTOCLAIM (exclusive `(N` bounds), XREAD (`BLOCK`/`STREAMS`), XREADGROUP, XCLAIM, XACK, XSETID, XINFO, XGROUP |
| Geo | 10 | GEOADD, GEOPOS, GEODIST, GEOHASH, GEORADIUS / GEORADIUS_RO, GEORADIUSBYMEMBER / GEORADIUSBYMEMBER_RO (with `STORE`/`STOREDIST` on the writable variants), GEOSEARCH, GEOSEARCHSTORE |
| HyperLogLog | 5 | PFADD, PFCOUNT, PFMERGE, PFDEBUG (`GETREG`/`DECODE`/`ENCODING`/`TODENSE`), PFSELFTEST |
| Bitmap | 7 | SETBIT, GETBIT, BITCOUNT, BITOP, BITPOS, BITFIELD (full bit field manipulation), BITFIELD_RO |
| Pub/Sub | 9 | SUBSCRIBE, UNSUBSCRIBE, PSUBSCRIBE, PUNSUBSCRIBE, PUBLISH, PUBSUB, SSUBSCRIBE, SUNSUBSCRIBE, SPUBLISH |
| Scripting | 8 | EVAL, EVALSHA, EVAL_RO, EVALSHA_RO, FCALL, FCALL_RO, FUNCTION, SCRIPT |
| Transactions | 5 | MULTI, EXEC, DISCARD, WATCH, UNWATCH |
| Server | 30+ | INFO, CONFIG (GET/SET/RESETSTAT/REWRITE/HELP), DBSIZE, FLUSHDB, FLUSHALL, SAVE, BGSAVE, BGREWRITEAOF, LASTSAVE, SWAPDB, SHUTDOWN, LATENCY, SLOWLOG, MONITOR, ROLE, COMMAND (COUNT/LIST/INFO/DOCS/GETKEYS/GETKEYSANDFLAGS), MEMORY (USAGE/STATS/DOCTOR), MODULE (LIST/LOAD/LOADEX/UNLOAD failure surfaces), LOLWUT, DEBUG, RESET, FAILOVER, WAIT, WAITAOF, TIME, REPLICAOF, SLAVEOF, READONLY, READWRITE, SYNC, PSYNC, REPLCONF, ASKING |
| Client | 19 | CLIENT SETNAME / GETNAME / ID / LIST / INFO / KILL / PAUSE / UNPAUSE / UNBLOCK / TRACKING / TRACKINGINFO / CACHING / GETREDIR / NO-EVICT / NO-TOUCH / SETINFO / REPLY / GET / HELP |
| Cluster | 20+ | CLUSTER INFO / MYID / SLOTS / SHARDS / NODES / KEYSLOT / RESET / ADDSLOTS / DELSLOTS / FLUSHSLOTS / FAILOVER / REPLICATE / REPLICAS / MEET / FORGET / SET-CONFIG-EPOCH / BUMPEPOCH / SAVECONFIG / LINKS / MYSHARDID / SLOTSTATE / COUNTKEYSINSLOT / GETKEYSINSLOT / COUNT-FAILURE-REPORTS / ADDSLOTSRANGE / DELSLOTSRANGE / SETSLOT (single-node mode; full multi-node sharding is not yet implemented) |
| Connection | 8 | AUTH, HELLO, PING, ECHO, SELECT, QUIT, CLIENT, RESET |
| ACL | 13 | All `ACL` subcommands listed above |
| Keys | 24 | DEL, EXISTS, TYPE, EXPIRE, EXPIREAT, PEXPIRE, PEXPIREAT, TTL, PTTL, EXPIRETIME, PEXPIRETIME, PERSIST, KEYS, RANDOMKEY, RENAME, RENAMENX, MOVE, COPY, TOUCH, UNLINK, DUMP, RESTORE, OBJECT (`ENCODING`/`REFCOUNT`/`IDLETIME`/`FREQ`/`HELP`), SCAN (`MATCH`/`COUNT`/`TYPE`) |
| Sort | 2 | SORT, SORT_RO (with `BY`/`GET`/`LIMIT`/`ALPHA`/`STORE` modifiers) |
| Migrate | 1 | MIGRATE (DUMP/RESTORE over TCP) |

---

## Persistence

### AOF

- Format: RESP-encoded command stream (compatible with stock Redis AOF readers for the implemented command set).
- Manifest-based multi-part layout: base RDB preamble + history files + incremental AOF files tracked in a manifest.
- `appendfsync everysec` / `always` / `no` (defaults to `everysec`).
- Replay policy: `AofReplayTailRepairPolicy::{Disabled, BoundedFinalSegment { max_tail_bytes }, HardenedNonAllowlisted}` selects how aggressively the server is allowed to recover from a torn tail; `BoundedFinalSegment` carries a `max_tail_bytes` cap so a malicious tail can't trigger unbounded repair.
- `BGREWRITEAOF` rewrites the AOF as a Store snapshot to a temp file and atomically swaps.
- AOF replay is fail-closed in strict mode: an unknown command inside a `MULTI` aborts the whole transaction rather than partially applying.

### RDB v11

- LZF compression on strings >20 bytes (pure-Rust port of Marc Lehmann's LZF, byte-for-byte compatible with vendored).
- Opcodes: `AUX` (0xFA), `SELECTDB` (0xFE), `RESIZEDB` (0xFB), `EXPIRETIME_MS` (0xFC), `EOF` (0xFF).
- CRC64 footer using the Redis polynomial.
- Encoder selects a compact upstream type tag per value shape (listpack/intset/hashtable/skiplist/quicklist).
- Decoder handles upstream type tags 11/16/17/18/19/20/21 including the two stream encodings.
- `FUNCTION DUMP` payloads are wrapped in an upstream version + CRC64 envelope so they round-trip through vendored servers.

### Eviction

`maxmemory-policy` covers `noeviction`, `allkeys-lru`, `allkeys-lfu`, `allkeys-random`, `volatile-lru`, `volatile-lfu`, `volatile-random`, `volatile-ttl`. LFU uses the upstream logarithmic counter with `LFU_INIT_VAL = 5` and a `lfu-log-factor` of 10.

---

## Replication

| Capability | State |
|---|---|
| `--replicaof` CLI bootstrap | ✓ |
| `REPLICAOF` / `SLAVEOF` from clients (incl. `REPLICAOF NO ONE`) | ✓ |
| `PSYNC` handshake (PING → AUTH → REPLCONF → PSYNC) | ✓ |
| `FULLRESYNC` snapshot streaming over TCP | ✓ |
| `CONTINUE` partial backlog replay | ✓ |
| `REPLCONF ACK` offset acknowledgement (no-direct-reply behavior) | ✓ |
| Replica reconnect with backoff | ✓ |
| `min-replicas-to-write` / `min-replicas-max-lag` write admission | ✓ |
| Replica-of-replica chaining | ✓ |
| `WAIT` / `WAITAOF` thresholds with saturating offset arithmetic | ✓ |
| `replica-priority` / `replica-announced` (for sentinel) | ✓ |
| Backlog rotation on `CONFIG SET repl-backlog-size` | ✓ |
| Authenticated upstream via `CONFIG SET masterauth` / `masteruser` | ✓ |

Both directions are exercised end-to-end in integration tests: vendored `redis-server` → FrankenRedis replica, and FrankenRedis primary → FrankenRedis replica → downstream FrankenRedis replica.

---

## Sentinel

The `fr-sentinel` crate is a clean-room reimplementation of Redis Sentinel, modeled on the same conceptual layers as `redis/src/sentinel.c`. Today it ships as a **library plus the `SENTINEL` command dispatcher**: the full state machine, discovery, health, consensus, and failover modules are present and unit/fuzz-tested, and the `SENTINEL` command surface is wired into the main runtime when `Store::sentinel_mode` is enabled. The remaining piece is a dedicated launcher binary that mirrors `redis-sentinel` and brings up a server in sentinel mode at startup.

```
┌──────────────────────────────────────────────────────────────────┐
│ fr-sentinel state                                                │
│                                                                  │
│   myid: [u8; 40]                                                 │
│   current_epoch: u64       (raft-like configuration epoch)       │
│   masters: HashMap<String, SentinelRedisInstance>                │
│                                                                  │
│   per-instance:                                                  │
│     - link, last_pub_time, last_hello_time                       │
│     - s_down_since_time, o_down_since_time, down_after_period    │
│     - role_reported, slave_repl_offset, slave_priority           │
│     - failover_state ∈ {None, WaitStart, SelectSlave,            │
│                         SendSlaveofNoone, WaitPromotion,         │
│                         ReconfSlaves, UpdateConfig}              │
│     - leader, leader_epoch, failover_epoch                       │
└──────────────────────────────────────────────────────────────────┘
```

| Component | Where |
|---|---|
| `__sentinel__:hello` pub/sub discovery (publish/parse `HelloMessage`) | `discovery.rs` |
| Periodic PING/INFO + S_DOWN evaluation | `health.rs` |
| Quorum-voted O_DOWN with vote staleness filtering | `consensus.rs` |
| Epoch-based leader election (`LeaderVote { voter_runid, leader_runid, epoch }`) | `consensus.rs` |
| 7-state failover machine, slave selection by priority → repl_offset → runid | `failover.rs` |
| `SENTINEL` command surface (MYID/MASTERS/MASTER/REPLICAS/SENTINELS/MONITOR/REMOVE/SET/RESET/GET-MASTER-ADDR-BY-NAME/CKQUORUM/FLUSHCONFIG/FAILOVER/PENDING-SCRIPTS/INFO-CACHE/DEBUG/HELP) | `commands.rs` |

The Sentinel parser surfaces are covered by their own fuzz target (`fuzz_sentinel_parsers`) and golden-artifact tests.

---

## ACL

Full lifecycle:

```bash
redis-cli ACL SETUSER alice on \
    '>s3cret' \
    '~cache:*' '~session:*' '-@all' '+@read' '+@string' '+set' '-del'
redis-cli ACL WHOAMI
redis-cli ACL LIST
redis-cli ACL DRYRUN alice GET cache:foo
redis-cli ACL LOG RESET
redis-cli ACL SAVE
```

| Feature | State |
|---|---|
| Per-command `+cmd` / `-cmd` | ✓ |
| Per-category `+@cat` / `-@cat` | ✓ |
| `allcommands` / `nocommands` / `allkeys` / `allchannels` / `reset` | ✓ |
| Key patterns `~pattern` | ✓ |
| Channel patterns `&pattern` | ✓ |
| Deny-first precedence at dispatch (`explicit deny > explicit allow > category deny > category allow`) | ✓ |
| ACL log with negative-count clamp | ✓ |
| `ACL SAVE` / `ACL LOAD` against an ACL file | ✓ |
| `%R` / `%W` / `%RW` key selectors (Redis 7.0 feature) | not yet (tracked as parity bead) |

### ACL precedence: a worked example

The deny-first precedence rule means each command/key check walks the user's rule list **in declaration order**, with explicit entries beating category entries and the last matching rule winning within each tier. The full priority is:

```
explicit deny  >  explicit allow  >  category deny  >  category allow  >  base policy
```

To make this concrete, consider:

```
ACL SETUSER alice on '>s3cret' \
    '~cache:*' '~session:*' \
    '-@all' '+@read' '+@string' '+set' '-del'
```

Now trace each request:

| Command attempt | Walk | Verdict |
|---|---|---|
| `GET cache:foo`     | `~cache:*` matches key. `+@read` allows GET (category allow). No later explicit/category deny touches GET. | **allowed** |
| `SET cache:foo bar` | Key matches. `+set` is an explicit allow (beats `-@all` category deny). | **allowed** |
| `DEL cache:foo`     | Key matches. `-del` is an explicit deny, top of the precedence stack. | **denied** |
| `SET foo bar`       | Key `foo` is **not in** `~cache:*`/`~session:*`, so it fails the key-pattern test before ACL even looks at commands. | **denied (NOKEY)** |
| `HGETALL session:42`| Key matches. HGETALL is in `@read`. `+@read` allows. | **allowed** |
| `FLUSHDB`           | FLUSHDB is in `@admin`/`@dangerous`. `-@all` denied at category level; no explicit allow. | **denied** |

`ACL DRYRUN alice <command>` runs exactly this walk without executing the command and replies with the same OK/error you'd actually get, which makes it ideal for staging permission changes. `ACL CAT <category>` lists what's in each category so you can see why `+@read` covers `HGETALL` but not `HSET`.

---

## Conformance harness

The `fr-conformance` crate is the single source of truth for parity.

```bash
# Run the in-process conformance suite (FrankenRedis runtime vs declared expectations)
cargo test -p fr-conformance -- --nocapture

# Run a single fixture in `command` mode against a live vendored Redis 7.2.4
# on host:port. The binary takes positional <mode> <fixture> [host] [port]
# plus optional --log-root / --json-out / --run-id / --case flags.
cargo run -p fr-conformance --bin live_oracle_diff -- \
    command crates/fr-conformance/fixtures/core_zset.json 127.0.0.1 6390

# Orchestrate the canonical matrix profile across the standard suites.
# --matrix accepts `baseline` (default), `parity`, or `all`.
cargo run -p fr-conformance --bin live_oracle_orchestrator -- --matrix parity
```

### What's inside

```
crates/fr-conformance/fixtures/
├── core_strings.json          (307 cases)   core_zset.json       (324 cases)
├── core_server.json           (282 cases)   core_stream.json     (273 cases)
├── core_scripting.json        (272 cases)   core_expiry.json     (256 cases)
├── core_generic.json          (232 cases)   core_list.json       (212 cases)
├── core_transaction.json      (192 cases)   core_config.json     (168 cases)
├── core_set.json              (164 cases)   core_connection.json (147 cases)
├── core_errors.json           (144 cases)   core_hash.json       (137 cases)
├── core_acl.json              (130 cases)   core_client.json     (127 cases)
├── core_blocking.json         (119 cases)   core_hyperloglog.json(119 cases)
├── core_object.json           (116 cases)   core_scan.json       (113 cases)
├── core_bitmap.json           (110 cases)   core_copy.json       (102 cases)
├── core_geo.json              (101 cases)   core_debug.json       (97 cases)
├── core_sort.json              (88 cases)   core_function.json    (87 cases)
├── core_cluster.json           (82 cases)   core_pubsub.json      (78 cases)
├── core_replication.json       (76 cases)   core_module_sentinel  (61 cases)
├── core_wait.json              (53 cases)   core_migrate.json     (51 cases)
├── core_pfdebug.json           (50 cases)
└── fr_p2c_*  packet journeys (event-loop, dispatch, ACL,
                                replication x2, cluster, expire/evict,
                                TLS — 8 fixture files)
```

Total: **4,975 cases across 43 fixtures**. Each case carries an `argv`, a `now_ms`, and an `ExpectedFrame` (`Simple` / `Error` / `Integer` / `Bulk` / `Array` / `SimplePattern` with `{hex40}` and `{int}` placeholders). Some carry threat-event expectations and structured-log contracts.

### Differential testing

The harness can spawn vendored `redis-server`, execute the same case on both runtimes via TCP, and compare RESP frames byte-for-byte. Field-ordering canonicalization keeps RESP3 Map/Set replies stable across runs. An exemption audit file (`live_oracle_audit_exemptions.json`, schema `live_oracle_audit_exemptions/v2`) lists the small number of fixtures intentionally **excluded from the live-oracle matrix** because they require specialized harnesses (multi-client blocking, dedicated replication-handshake harness, dedicated persist-replay path, TLS-enabled oracle); each exemption records its replacement coverage.

CI runs the full live conformance suite on every push to `main` (`.github/workflows/live-conformance-gates.yml`).

---

## Fuzzing

33 `cargo-fuzz` targets live under `fuzz/`. They are run continuously and seed corpora are checked in.

| Target group | Targets |
|---|---|
| Wire protocol | `fuzz_resp_parser`, `fuzz_resp_roundtrip`, `fuzz_resp_configured_sequences`, `fuzz_inline_parser`, `fuzz_command_parse`, `fuzz_command_parse_advanced`, `fuzz_command_option_parsers` |
| RDB | `fuzz_rdb_encode_round_trip`, `fuzz_rdb_decoder`, `fuzz_rdb_structured` |
| AOF | `fuzz_aof_decoder`, `fuzz_aof_manifest_parser` |
| Data structures | `fuzz_store_bitops`, `fuzz_store_hll`, `fuzz_store_scan_family`, `fuzz_store_stream_groups`, `fuzz_keyspace_events` |
| Runtime + VM | `fuzz_runtime_sequence`, `fuzz_runtime_execute_bytes`, `fuzz_differential_runtime`, `fuzz_eventloop_validators` |
| Scripting | `fuzz_lua_eval`, `fuzz_function_restore` |
| Replication + protocol | `fuzz_psync_reply`, `fuzz_migrate_request`, `fuzz_dump_restore`, `fuzz_client_tracking` |
| Sentinel | `fuzz_sentinel_parsers` |
| Misc | `fuzz_client_reply`, `fuzz_glob_match`, `fuzz_config_file`, `fuzz_tls_config`, `fuzz_acl_rules` |

```bash
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run fuzz_resp_parser
```

---

## Observability

FrankenRedis emits the same observability surface Redis operators are used to, plus a few additions driven by the threat-event ledger.

### `INFO`

`INFO` returns these sections (use `INFO server` etc. to scope, or `INFO all` for everything):

| Section | What's in it |
|---|---|
| `server` | `redis_version` (advertised as `7.2.4` for client compatibility), `run_id`, `tcp_port`, `process_id`, `uptime_in_seconds`, `uptime_in_days`, `arch_bits`, `os`, etc. ~21 fields |
| `clients` | `connected_clients`, `cluster_connections`, `maxclients`, `client_recent_max_input_buffer`, `client_recent_max_output_buffer`, `blocked_clients`, etc. |
| `memory` | `used_memory`, `used_memory_rss` (read from `/proc/self/status`), `used_memory_peak`, `maxmemory`, `maxmemory_policy`, `mem_fragmentation_ratio`, etc. |
| `persistence` | `loading`, `rdb_changes_since_last_save` (dirty counter), `rdb_bgsave_in_progress`, `rdb_last_save_time`, `aof_enabled`, `aof_rewrite_in_progress`, `aof_current_size`, etc. |
| `stats` | `total_connections_received`, `total_commands_processed`, `instantaneous_ops_per_sec`, `total_net_input_bytes`, `total_net_output_bytes`, `rejected_connections`, `expired_keys`, `evicted_keys`, etc. |
| `cpu` | `used_cpu_sys`, `used_cpu_user`, `used_cpu_sys_children`, `used_cpu_user_children` |
| `replication` | `role`, `connected_slaves`, `master_replid`, `master_repl_offset`, plus per-replica lines and `min_slaves_to_write` / `min_slaves_max_lag` mirroring upstream |
| `commandstats` | per-command `calls`, `usec`, `usec_per_call`, `rejected_calls`, `failed_calls` |
| `errorstats` | per-error-prefix counts (e.g., `errorstat_WRONGTYPE:count=N`) |
| `latencystats` | per-event latency-monitor entries |
| `keyspace` | per-database `keys=N,expires=N,avg_ttl=N` |

### `SLOWLOG` and `LATENCY`

`SLOWLOG GET [n]` returns the last `slowlog-max-len` commands whose execution exceeded `slowlog-log-slower-than` microseconds, each with a unique id, a client name/address, and the full argv. `LATENCY HISTORY <event>` and `LATENCY DOCTOR` work the same way they do upstream and are wired through real timing instrumentation around the dispatch path.

### `MONITOR`

`MONITOR` streams every successful command back to the subscribing client in the canonical `<unix-ts.us> [<db> <client>] <argv...>` format, exactly as Redis does. Use it to debug client traffic in dev; never enable it in production-hot paths.

### `CLIENT LIST` / `CLIENT INFO`

`CLIENT LIST` returns one line per connected client with the standard fields: `id addr laddr fd name age idle flags db sub psub ssub multi qbuf qbuf-free argv-mem multi-mem rbs rbp obl oll omem tot-mem events cmd user redir resp lib-name lib-ver`. The flag string preserves the upstream alphabet (`e` evicted, `T` no-touch, `t` keys-tracking, `B` blocked, `P` pubsub, `r` readonly, `x` MULTI/EXEC, …) so existing dashboards and parsers keep working.

### Keyspace notifications

`CONFIG SET notify-keyspace-events <flags>` (e.g. `AKE` for all-keyspace + all-key-events) wires Redis-style `__keyspace@<db>__:<key>` and `__keyevent@<db>__:<event>` publication. Event names match upstream for `BITOP`, `PFADD`, `*STORE`, `XGROUP`, the blocking-pop family, and the rest, all pinned during the April–May parity sweeps.

### Threat-event ledger

In hardened mode, every `ThreatEvent` (see Implementation deep-dives → Threat-event ledger format) is appended to a deterministic JSON-lines ledger. The schema is defined in `TEST_LOG_SCHEMA_V1.md` (`STRUCTURED_LOG_SCHEMA_VERSION = fr_testlog_v1`). Each line carries the timestamp, the threat class, a SHA256 digest of the offending input, the decision action, and a one-line reason; enough to reconstruct what hardened mode did without rerunning the workload.

---

## Embedding `fr-*` crates in your own project

The workspace is structured so the most useful internals can be lifted into other Rust projects without depending on the full server. Common patterns:

### Parse and emit RESP without running a server

```toml
# Cargo.toml
[dependencies]
fr-protocol = { path = "../frankenredis/crates/fr-protocol" }
```

```rust
use fr_protocol::{ParseResult, ParserConfig, RespFrame, parse_frame_with_config};

let cfg = ParserConfig {
    max_bulk_len: 16 * 1024 * 1024,   // 16 MiB per bulk
    max_array_len: 1_000_000,
    max_recursion_depth: 32,
    allow_resp3: true,                // opt-in; default is false (fail-closed)
};

let ParseResult { frame, consumed } =
    parse_frame_with_config(b"*1\r\n$4\r\nPING\r\n", &cfg)?;
assert_eq!(consumed, 14);
assert!(matches!(frame, RespFrame::Array(_)));

// Encoding is a method on RespFrame:
let mut out = Vec::with_capacity(64);
RespFrame::SimpleString("PONG".into()).encode_into(&mut out);
assert_eq!(out, b"+PONG\r\n");
```

This gives you a hardened RESP2/3 codec with CRLF-injection sanitization and configurable limits, useful for proxies, gateways, or anywhere you need to speak RESP without owning a key/value store. The default `ParserConfig::default()` keeps `allow_resp3 = false` to preserve the fail-closed posture on untrusted input.

### Use the data engine as a typed in-memory store

```toml
[dependencies]
fr-store = { path = "../frankenredis/crates/fr-store" }
```

```rust
use fr_store::Store;

let mut store = Store::new();
let now_ms: u64 = 1_778_889_600_000;

// SET hello world (no TTL):
store.set(b"hello".to_vec(), b"world".to_vec(), None, now_ms);
let v = store.get(b"hello", now_ms).unwrap().unwrap();
assert_eq!(v, b"world");

// SET with a 5-second PX TTL:
store.set(b"session:42".to_vec(), b"alice".to_vec(), Some(5_000), now_ms);

// ZSet with full upstream-compatible ordering (total_cmp on f64, lex tie-break):
store
    .zadd(
        b"leaderboard",
        &[(1.0, b"alice".to_vec()), (3.5, b"bob".to_vec())],
        now_ms,
    )
    .unwrap();
let asc = store.zrange(b"leaderboard", 0, -1, now_ms).unwrap();
// asc == ["alice", "bob"];  use zrevrange for descending.
```

Most `Store` mutators take a trailing `now_ms: u64` because expiry, LFU decay, and `OBJECT IDLETIME` all consult it. Pass a real wall-clock time in milliseconds, or a deterministic value if you're driving from a test harness.

### Replay RDB or AOF outside the server

```toml
[dependencies]
fr-persist = { path = "../frankenredis/crates/fr-persist" }
```

```rust
use fr_persist::{decode_rdb, decode_aof_stream};

let bytes = std::fs::read("dump.rdb")?;
let (entries, aux) = decode_rdb(&bytes)?;
println!("snapshot taken by redis-ver = {:?}", aux.get("redis-ver"));
for entry in entries {
    println!("db={} key={:?} expire_ms={:?}",
             entry.db, entry.key, entry.expire_ms);
}

let aof_bytes = std::fs::read("appendonly.aof")?;
let records = decode_aof_stream(&aof_bytes)?;
for rec in records {
    println!("AOF: {:?}", rec.argv);
}
```

Both decoders accept vendored Redis 7.2.4 files unmodified. Handy for forensic tooling, migrations, or building an offline RDB inspector.

### Drive the runtime without owning a socket

```toml
[dependencies]
fr-runtime = { path = "../frankenredis/crates/fr-runtime" }
fr-config  = { path = "../frankenredis/crates/fr-config" }
fr-protocol = { path = "../frankenredis/crates/fr-protocol" }
```

```rust
use fr_config::RuntimePolicy;
use fr_runtime::Runtime;

let mut rt = Runtime::new(RuntimePolicy::default());          // strict mode
// or:  Runtime::new(RuntimePolicy::hardened())               // hardened mode

let now_ms: u64 = 1_778_889_600_000;
let reply = rt.execute_bytes(
    b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
    now_ms,
);
assert!(reply.ends_with(b"+OK\r\n"));
```

`Runtime` exposes `execute_bytes`, `execute_frame`, and `execute_frame_with_unix_time_us` for tests, embedded use, or building alternative front-ends (UDP gateway, Unix-socket-only deployments, HTTP/JSON adapter). `RuntimePolicy::default()` selects strict mode with an empty hardened allowlist; `RuntimePolicy::hardened()` selects hardened mode pre-loaded with `HARDENED_ALLOWLIST_DEFAULT`.

### Build a custom benchmark harness

`fr-bench` is fully scriptable and `cargo install --path crates/fr-bench` will drop a `fr-bench` binary in your `~/.cargo/bin`. Workloads are normalized to a `frankenredis_baseline/v1` JSON schema you can diff against any vendored or upstream baseline.

---

## Benchmarking methodology

The Performance table above is the surface report; this is how the harness actually produces those numbers.

### What `fr-bench` measures

`fr-bench` is a TCP load generator written specifically for FrankenRedis but speaking standard RESP2, so it works against any Redis-compatible server. For each workload it:

1. Opens `--clients` TCP connections (default: 50) and authenticates if needed.
2. Pre-fills the keyspace where the workload requires it (e.g. `LPOP` needs lists populated).
3. Drives `--requests` total operations (default: 100,000) round-robin across the connections.
4. For each request, samples `(send_unix_us, recv_unix_us)` and records `(recv − send)` into a `Histogram<u64>` (HdrHistogram, max value 60 s) at microsecond precision.
5. At the end, computes `p50`, `p95`, `p99`, `p999`, `max`, `mean`, `samples`, plus `ops_per_sec = requests / total_time_sec`, `bytes_sent`, and `bytes_received`.

Results are normalized into the `frankenredis_baseline/v1` JSON schema. Eight workload variants ship:

| Workload | Shape |
|---|---|
| `set`   | `SET key value` with a 3-byte payload (configurable via `--datasize`) |
| `get`   | `GET key` (after pre-fill) |
| `incr`  | `INCR key` |
| `lpush` | `LPUSH key value` |
| `lpop`  | `LPOP key` (after pre-fill of `requests / keyspace / pipeline` per key) |
| `hset`  | `HSET key field value` |
| `hget`  | `HGET key field` (after pre-fill) |
| `mixed` | 50% `GET` / 50% `SET` blend |

### Pipelining

`--pipeline N` sends `N` RESP commands per round-trip without waiting. That mode produces the `pipeline=16` numbers in the Performance table. Pipelined latency is intentionally higher per-request (the request waits for `N−1` siblings to also clear the queue) but throughput goes up substantially: Redis can do nearly an order of magnitude more ops/sec at `pipeline=16` than at `pipeline=1`.

### Why HdrHistogram

Mean and median latency are misleading for server workloads; the long tail is where user-visible jitter actually comes from. HdrHistogram lets you read `p99`, `p99.9`, and `max` with constant-time queries on a fixed-memory data structure with bounded relative precision. The harness uses microsecond buckets up to 60 s, which is enough headroom that no real request ever clips the top of the histogram (it would mean a 60-second timeout).

### The regression gate

`./scripts/benchmark_gate.sh` runs fresh `fr-bench` captures, compares them to the checked-in baselines, and exits non-zero if any workload regresses past the configured thresholds:

```bash
FR_BENCH_THROUGHPUT_DROP_PCT=10   # fail if throughput drops more than 10%
FR_BENCH_P99_REGRESSION_PCT=20    # fail if p99 grows more than 20%
./scripts/benchmark_gate.sh
```

Each run writes raw per-workload reports, side-by-side comparisons, and an aggregate gate report under `artifacts/benchmark/<run-id>/`. The intent is that any optimization PR can attach the artifacts directory and let a reviewer see "what changed, by how much, and whether it stayed inside the gate."

### Isomorphism proofs

A perf change can be a regression even if every benchmark improves, if it changed observable behavior. Each optimization round therefore produces an `ISOMORPHISM_PROOF_*.md` artifact under `artifacts/optimization/` that argues two things:

1. **Behavior unchanged.** The differential conformance suite still passes against vendored Redis after the change.
2. **The change is the cause.** A before/after `perf record` flamegraph and an `strace` syscall trace show the hotspot moved where the proof says it did.

The lazy `Store::state_digest` + ACL category short-circuit rounds in `artifacts/optimization/throughput-gap/ISOMORPHISM_PROOF_LAZY_DIGEST.md` are a worked example.

---

## Threat model summary

FrankenRedis defends against five concrete attacker postures. The full matrix is in [`SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md`](SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md); the abbreviated form:

| Attacker | What they try | What FrankenRedis does |
|---|---|---|
| **Hostile RESP client** | Oversized bulks, deeply nested arrays, recursion bombs, CRLF injection in error bodies, unknown RESP3 type prefixes | Parser enforces `max_bulk_len`/`max_array_len`/`max_recursion_depth` ceilings; error/string bodies are sanitized for CRLF; unknown RESP3 types in strict mode are rejected as `UnsupportedResp3Type` (fail-closed) |
| **Hostile inline command** | Unbalanced quotes, embedded newlines, malformed numeric arguments | Inline parser returns a RESP error for unbalanced quotes; numeric arguments are parsed with strict integer / float parsers that reject whitespace padding and underflow |
| **Persistence tampering** | RDB CRC64 mismatch, AOF tail with mid-record CRLF, LZF payload claiming impossible decompressed size | RDB CRC mismatch fires `PersistenceTampering` → `FailClosed`; AOF tail repair policy is operator-chosen (`Disabled`/`BoundedFinalSegment`/`HardenedNonAllowlisted`); LZF decoder bounds-checks the claimed length before allocating |
| **Replication order attack** | Out-of-order replication offsets from a forged primary; replica that lies about its run-id; backlog payload with a stitched-in command stream | Offset arithmetic is saturating; FSM rejects PSYNC continuations whose `(repl_id, offset)` doesn't match the local backlog window; `min-replicas-to-write` write admission denies writes when fewer than N replicas have ACK'd within `min-replicas-max-lag` |
| **Auth/ACL confusion** | Privilege escalation via `ACL SETUSER` modifier ordering, deprecated `+@all` after explicit `-cmd`, password reset via `resetpass` ordering | Deny-first precedence at dispatch; explicit-allow after `+@all` preserves the explicit; `resetpass` no longer enables passwordless login on accident; ACL log records every rejected attempt |

In strict mode, the default decision for every threat class is `FailClosed` with severity `S0`; the server never silently bounded-defends. In hardened mode, the operator can allowlist a small set of categories (`BoundedParserDiagnostics`, `BoundedReplayRepair`, `ResourceClamp`, `MetadataSanitization`) where bounded defense is preferable to a hard fail. Every defended event is recorded in the threat-event ledger so the operator sees what happened.

### What FrankenRedis does NOT defend against

- **CPU exhaustion via expensive but legal commands.** A determined adversary with `+KEYS` permission can still ask `KEYS '*'` on a giant keyspace; that's a workload problem, not a security problem. Use ACL to scope what each user can do.
- **TLS interception.** Wire-level TLS is not yet terminated (see Limitations); front the server with `stunnel`/`spiped` or terminate at your load balancer until rustls wiring lands.
- **Multi-tenant isolation across databases.** `SELECT 0..15` provides Redis-style logical separation but not security separation; an attacker with `+SELECT` can move between databases. Use separate FrankenRedis processes or ACL `~prefix:*` keyspaces for hard isolation.
- **DoS via UDP / ICMP / kernel-level attacks.** That's a network-layer concern; FrankenRedis is a TCP application.

---

## Comparison vs alternatives

| | FrankenRedis | Redis 7.2.4 | KeyDB | Dragonfly | Microsoft Garnet | Valkey |
|---|---|---|---|---|---|---|
| Language | Rust | C | C++ | C++ | C# / .NET | C |
| Memory safety | `#![forbid(unsafe_code)]` | manual | manual | manual | runtime + GC | manual |
| Drop-in protocol parity goal | Redis 7.2.4 byte-exact | (oracle) | Redis fork | RESP-compatible subset | RESP-compatible subset | Redis fork |
| Threading model | single-threaded mio loop | single-threaded | multi-threaded | multi-threaded shared-nothing | multi-threaded | single-threaded |
| Differential test against canonical Redis | **yes (live diff in CI)** | n/a | no | no | no | implicit |
| First-class strict/hardened mode split | **yes** | no | no | no | no | no |
| Built-in Sentinel | **yes (`fr-sentinel`)** | yes (external binary) | yes | no (Raft built-in) | no | yes |
| Cluster sharding | not yet | yes | yes | yes | partial | yes |
| RaptorQ durability sidecar | planned (not implemented) | no | no | no | no | no |
| License | MIT | BSD-3 (≤7.2), then dual SSPLv1 / RSALv2 (7.4+) | BSD-3 | BSL 1.1 | MIT | BSD-3 (Linux Foundation Redis 7.2.4 fork) |

**Position.** FrankenRedis does not aim to be faster than Redis or to multithread it. The goal is the same observable behavior with `unsafe` removed, a real strict/hardened policy split, an explicit threat-event ledger, and a clean enough internal model that the data engine, RDB codec, replication FSM, and Lua evaluator can be embedded into other Rust projects.

---

## Limitations

Honest list of what FrankenRedis does *not* do today. The roadmap below tracks closure.

- **No multi-node cluster sharding.** The `CLUSTER` command surface is implemented for single-node mode (slot map, NODES, INFO, KEYSLOT, etc.), but FrankenRedis does not yet do CRC16 slot rebalancing or live shard migration across multiple FrankenRedis processes.
- **Pipelined throughput trails Redis at high pipeline depth.** Single-command throughput is in the 71–83% range of Redis; `pipeline=16` is at ~33–47%. The `writev` scatter-gather work that closes the gap is on the roadmap.
- **Wire-level TLS not yet terminated.** TLS configuration, accept-rate-limit, and policy are wired through `fr-config` / `fr-runtime`, but the listener does not yet terminate `rustls` connections. Clients connect in plaintext for now.
- **Hash field TTL: storage layer only.** The in-memory representation (`hash_field_expires: BTreeMap<(key, field), expires_at_ms>` on `Store`) plus the `RDB_TYPE_HASH_WITH_TTLS` (tag 100) round-trip both exist, but the full Redis 7.4 wire-level command family — `HEXPIRE`, `HPEXPIRE`, `HEXPIREAT`, `HPEXPIREAT`, `HEXPIRETIME`, `HPEXPIRETIME`, `HPERSIST`, `HTTL`, `HPTTL` — is not yet dispatched, and the lazy-expiry enforcement at every hash read path is still to come.
- **HyperLogLog representation is always dense.** Upstream uses a sparse representation for low cardinalities; FrankenRedis uses the 16,389-byte dense form unconditionally. Tracked as `frankenredis-j2tuo`.
- **`DUMP` for large quicklist entries uses the PACKED container.** Upstream Redis emits a PLAIN container compressed with LZF for big-item quicklist nodes; FrankenRedis currently emits PACKED. The on-wire payload remains a valid `DUMP`/`RESTORE` round-trip in both directions, but is not byte-identical to vendored. Tracked as `frankenredis-371l9`.
- **Maxmemory eviction is exact-scan, not sample-based.** `select_eviction_candidate` walks every `Entry` to find the best LRU/TTL candidate (`O(N)` per eviction). Upstream samples `maxmemory-samples` random keys and merges into an `EVPOOL_SIZE = 16` pool. `CONFIG SET maxmemory-samples` is accepted for compatibility but doesn't yet influence selection.
- **LFU eviction selection currently falls back to LRU.** The 8-bit LFU counter is tracked per-Entry and exposed correctly via `OBJECT FREQ` (with upstream-shape logarithmic increment + minute-decay), but `select_eviction_candidate` for `allkeys-lfu` / `volatile-lfu` picks by `last_access_ms` rather than by `lfu_freq` today.
- **`ACL` does not yet implement `%R`/`%W`/`%RW` key selectors.** The Redis 7.0 fine-grained key-selector syntax is on the roadmap. Tracked as `frankenredis-y40p3`.
- **Lua: small stdlib gaps.** `cmsgpack`, `struct`, `setfenv`/`getfenv`, and `newproxy` are not exposed to scripts. `cjson.decode` is permissive about trailing commas / non-string keys where upstream rejects them. Each is tracked as a P4 parity bead.
- **RaptorQ-everywhere durability sidecar is not started.** The doctrine is named in `AGENTS.md`; no crate dependency or implementation exists yet.
- **No tagged releases.** Workspace version is `0.1.0` everywhere; the project is pre-1.0 and `main` is the only branch with guarantees.

---

## Roadmap

1. **Multi-node cluster sharding**: CRC16 slot allocation, slot migration, MOVED/ASK redirects, gossip.
2. **`writev` scatter-gather** for the write path, to close the `pipeline=16` gap to Redis.
3. **Wire-level `rustls` termination** in `fr-server`, to fully realize the TLS configuration framework that already exists in `fr-config`/`fr-runtime`.
4. **Hash field TTL lazy-read enforcement** across every read path.
5. **HyperLogLog sparse representation** for low cardinalities.
6. **ACL `%R`/`%W`/`%RW` key selectors** (Redis 7.0 fine-grained selectors).
7. **RaptorQ-everywhere sidecar** for self-healing durability of long-lived state snapshots, fixture bundles, and reproducibility ledgers.
8. **Asupersync-backed runtime adapter** and **FrankenTUI operator dashboard adapter** for the deployment story.

---

## Developer workflow

Patterns we've found work for extending FrankenRedis without breaking parity.

### Adding a new command

1. **Find the upstream behavior first.** Read the relevant block in `legacy_redis_code/redis/src/<family>.c` and any matching `tests/unit/type/<family>.tcl`. The conformance contract is "do whatever vendored does on the same inputs, byte for byte."
2. **Add a match arm in `crates/fr-command/src/lib.rs::dispatch_argv`.** Use `eq_ascii_command(cmd, b"NEWCMD")` and parse the argv tail with the existing option-parser helpers (`parse_signed_int_arg`, `parse_f64_arg`, `parse_key_arg`, and so on).
3. **Add the corresponding `Store` method in `crates/fr-store/src/lib.rs`.** Mutators must take `now_ms: u64`, bump `modification_count`, increment the dirty counter, and trigger `capture_aof_record` from the runtime side. Read-only methods follow the `&self`-or-`&mut self`-with-`record_keyspace_lookup` pattern.
4. **Wire the AOF capture in `fr-runtime`.** If the command mutates state, add the argv to the AOF-records list before returning. If it's read-only, don't.
5. **Add fixtures in `crates/fr-conformance/fixtures/core_<family>.json`.** Each case has an `argv`, a `now_ms`, and an `ExpectedFrame`. Run the case against vendored Redis to capture the expected reply; never hand-write expectations.
6. **Add a fuzz target** if the new command has a non-trivial parser surface (`fuzz/fuzz_targets/fuzz_<family>.rs`).
7. **Run the validation suite:**

```bash
cargo fmt --check
cargo check  --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test   --workspace
cargo test -p fr-conformance -- --nocapture
```

### Adding a conformance fixture from observed vendored replies

The discipline for new fixtures is "never invent expectations, observe them." The canonical workflow:

1. Start vendored Redis 7.2.4 on a known port:
   ```bash
   legacy_redis_code/redis/src/redis-server --port 6390 --save "" --appendonly no
   ```
2. Run the case through `redis-cli` (or a small Rust harness) against that
   port and capture the wire reply byte-for-byte.
3. Translate the observed reply into the JSON fixture's `ExpectedFrame`
   shape (`Simple`/`Error`/`Integer`/`Bulk`/`Array`, or `SimplePattern`
   with `{hex40}` / `{int}` placeholders when the reply contains
   run-id-ish substrings).
4. Add the case to the appropriate `core_<family>.json` and re-run
   `cargo test -p fr-conformance` plus a `live_oracle_diff` against the
   same fixture to confirm the new case passes on both runtimes.

The repeating "probe sweep" workflow (described below) is the automated
form of the same loop.

### Running a probe sweep

A "probe sweep" is the workflow that drove most of Phase 11. The shape:

1. **Generate adversarial command sequences.** Empty argv, wrong arity, mid-multi nested EVAL, oversized bulks, weird option ordering, deprecated synonyms.
2. **Run each sequence against both servers** via `live_oracle_diff`.
3. **For every divergence,** open a beads issue with the argv, both replies, and the suspected root cause:

```bash
br create --type bug --priority 4 --title \
    "BLMPOP returns WRONGTYPE on non-list key (frankenredis-oks10)"
br dep add frankenredis-oks10 frankenredis-probe-sweep-N
```

4. **Close the issue with the fix.** Every commit that closes a parity bead lands with `(frankenredis-<slug>)` in the subject so `git log` traces back to the original divergence.
5. **Document the sweep** with a final `chore(beads): file follow-ups from probe sweep #N` commit listing whatever follow-ups got created from the run.

The CHANGELOG's Phase 11 section is largely the output of this workflow. The methodology is repeatable: every new sweep finds a smaller batch of remaining gaps.

### Debugging cheat-sheet

| Goal | Tool |
|---|---|
| What encoding does this key use? | `OBJECT ENCODING <key>` |
| How idle is this key? | `OBJECT IDLETIME <key>` |
| How often is this key accessed (LFU)? | `OBJECT FREQ <key>` |
| What is in this key (raw value)? | `DEBUG OBJECT <key>` (requires `enable-debug-command yes`) |
| Trace every command flowing through the server | `MONITOR` |
| What was the slowest recent command? | `SLOWLOG GET 10` |
| Where is latency budget spent? | `LATENCY HISTORY <event>` + `LATENCY DOCTOR` |
| What's the actual replication state? | `INFO replication` |
| What's the actual memory pressure? | `INFO memory` and `MEMORY STATS` |
| Force a synchronous save? | `SAVE` (blocks the loop) or `BGSAVE` (forks a child) |
| Reload the AOF without restarting? | `DEBUG LOADAOF` (requires `enable-debug-command yes`) |
| Run a single fixture case against both runtimes | `cargo run -p fr-conformance --bin live_oracle_diff -- --case <case_name> command <fixture.json> 127.0.0.1 6390` |
| Re-verify a probe-sweep hunch | `cargo run -p fr-conformance --bin adversarial_triage -- --manifest <path>` (see `--help`) |

### What runs in CI

`.github/workflows/live-conformance-gates.yml` is the source of truth. The actual workflow runs `ubuntu-latest`, installs Redis via apt-get plus Rust nightly via `dtolnay/rust-toolchain@nightly`, and then drives these gates in order:

- **G1 — Verify Formatting:** `cargo fmt --check`.
- **G1 — Verify Lints:** `cargo clippy --workspace --all-targets -- -D warnings`.
- **G2 — Unit and Property Baseline:** `cargo test --workspace` plus property-test sweeps.
- **G3 + G5 — Live Oracle Differential Suites:** the CI step shells out to `./scripts/run_live_oracle_diff.sh` which drives the `live_oracle_orchestrator` binary (`cargo run -p fr-conformance --bin live_oracle_orchestrator`) against the already-running `redis-server` on 127.0.0.1:6379. Every fixture under `crates/fr-conformance/fixtures/` is diffed reply-by-reply. Fail if a divergence appears that isn't already in `live_oracle_audit_exemptions.json`.
- **G7 — Coverage / Flake Budgets:** `scripts/check_coverage_flake_budget.sh`.
- **G4 — Adversarial Triage:** run the adversarial corpus against both servers and classify any divergences.
- **G6 — Optimization Gate:** `phase2c_schema_gate --optimization-gate`.
- **G6 — Benchmark Regression Gate** *(opt-in via the `run_benchmark_gate` workflow_dispatch input)*: `scripts/benchmark_gate.sh` against checked-in baselines.
- **G7 — User Workflow Corpus Gate:** the `user_journey_corpus_gate` binary.
- **G7 — Packet Schema Gate:** `phase2c_schema_gate` against the extraction packets in `artifacts/phase2c/`.
- **G8 — RaptorQ Artifact Gate:** verify the durability-artifact schema (the framework is wired even though the RaptorQ sidecar itself is still on the roadmap).
- **G7 — Failure Forensics Index:** any failed gate above contributes to a forensics bundle under `artifacts/failure_forensics/<run-id>/`.

Each major stage (live oracle, adversarial triage, raptorq, failure forensics, benchmark regression) uploads its artifact bundle so a reviewer can inspect the full reply / argv / digest log without re-running CI locally.

A PR cannot merge to `main` unless the gates pass and there are no new exemption entries.

---

## Operating practices

A few patterns we've found useful for running FrankenRedis in real scenarios.

### Migrating from vendored Redis

FrankenRedis reads vendored Redis 7.2.4 RDB files unmodified, so the simplest migration is:

```bash
# 1. On the legacy host, take a fresh snapshot.
redis-cli BGSAVE
# wait for rdb_bgsave_in_progress=0 in INFO persistence

# 2. Copy dump.rdb to the new host.
scp /var/lib/redis/dump.rdb new-host:/var/lib/frankenredis/dump.rdb

# 3. Start FrankenRedis with --rdb only. RDB auto-load on startup runs
#    only when --aof is NOT passed (matching Redis: AOF takes precedence
#    over RDB at boot if both are configured, so passing --aof against an
#    empty AOF file would start the server empty).
./target/release/frankenredis --port 6379 \
    --rdb /var/lib/frankenredis/dump.rdb

# 4. If you want AOF durability going forward, enable it after startup
#    so the in-memory state from the RDB gets persisted into the new AOF
#    via the implicit BGREWRITEAOF that CONFIG SET triggers.
#
#    Note: `dir` is a PROTECTED config in Redis 7.x, so it can only be
#    set at startup via a config file (or by first enabling
#    enable-protected-configs). The clean path is to start with
#    --config redis.conf where redis.conf contains `dir /var/lib/...`.
redis-cli -p 6379 CONFIG SET appendonly yes
```

For zero-downtime cutover, run FrankenRedis as a **replica** of the legacy primary first, let it fully catch up (`INFO replication` → `master_link_status:up`, `master_last_io_seconds_ago:0`), then issue `REPLICAOF NO ONE` on the FrankenRedis side and repoint your clients. Both directions have been integration-tested.

### Running alongside vendored Redis for differential validation

The conformance harness is the supported way to run differential checks locally:

```bash
# In one terminal: start vendored Redis on a non-standard port
legacy_redis_code/redis/src/redis-server --port 6390 --save "" --appendonly no

# Then in another: run a single fixture against both runtimes and diff replies.
# Positional args after `--`: <mode> <fixture> [host] [port].
cargo run -p fr-conformance --bin live_oracle_diff -- \
    command crates/fr-conformance/fixtures/core_strings.json 127.0.0.1 6390
```

The harness reports any wire-byte differences with the case index, the argv, and the two replies side by side, which is exactly what `(frankenredis-<slug>)` commits typically reference.

### Backup and disaster recovery

The combination of AOF (`appendfsync everysec`) and periodic RDB (`save 3600 1`) gives you a 1-second loss bound on the AOF and an hour-bounded full-snapshot for fast cold restore. For DR specifically:

- The RDB CRC64 footer guarantees that a corrupted file fails to load instead of loading partially.
- The AOF manifest tracks base + history + incremental files so an interrupted `BGREWRITEAOF` doesn't lose the live AOF stream.
- Hash field TTLs round-trip through RDB, so per-field expirations survive snapshots.

### Capacity and tuning notes

- **`hz`** (default 10) controls how often the active-expire cycle runs per second. Raise it if you have many short-TTL keys; lower it on memory-constrained hosts that don't care about prompt expiry. The upstream `active-defrag-*` keys are accepted by `CONFIG SET` for parity but FrankenRedis does not yet run an active defragmentation cycle, so they have no observable effect today.
- **`repl-backlog-size`** should be at least `(peak write throughput bytes/sec × tolerable disconnect seconds)`. The default `1mb` is too small for most production workloads.
- **`client-output-buffer-limit`** for replicas should be generous; if a slow replica is killed for OBL overflow you'll see a FULLRESYNC storm next time it reconnects.
- **`maxmemory-samples`** of 5 is sufficient for `allkeys-lru`/`volatile-lru`; raise to 10 for `allkeys-lfu`/`volatile-lfu` where counter accuracy matters more.

### Choosing strict vs hardened

| You're running… | Use |
|---|---|
| Drop-in replacement for an internal Redis where clients expect byte-exact replies | `--mode strict` |
| Internet-exposed service or one accepting input from untrusted producers | `--mode hardened` and keep the ledger |
| Conformance / differential testing | `--mode strict` (never hardened, or your diff against vendored is meaningless) |
| Side-by-side staging during migration | `--mode strict` so you can confirm byte-exact parity, then switch to hardened in production |

---

## FAQ

### Is FrankenRedis production-ready?

No. The workspace is `0.1.0` and there are no tagged releases. The pieces that are implemented are exercised by 4,975 differential conformance cases and 33 fuzz targets, but you should not yet stake a production system on it without your own validation, particularly around sharding (which doesn't exist yet) and TLS (which is configured but not wire-terminated).

### Does it speak the regular Redis protocol?

Yes. RESP2 is native. RESP3 inbound parsing is supported, and RESP3 `Map`/`Set` replies are emitted for the appropriate commands when the client negotiates `HELLO 3`. Any client that talks to Redis 7.2.4 talks to FrankenRedis.

### Will my Lua script work?

If it works on Redis 7.2.4, very likely yes. The custom Lua 5.1 evaluator implements `redis.call`/`pcall`/`status_reply`/`error_reply`/`sha1hex`/`log`/`replicate_commands`/`set_repl`/`setresp`/`acl_check_cmd`/`breakpoint`/`debug`, the full pattern matcher including `%b`/`%f`/`%1`–`%9` back-references, the script-relevant metamethod family (`__index`/`__newindex`/`__call`/`__concat`/`__add` family/`__eq`/`__lt`/`__le`/`__tostring`/`__unm`/`__metatable`), the LuaJIT-compatible `bit` library, `cjson.encode`/`decode` with upstream `%.14g` formatting, closures with upvalue capture, and coroutines. Known gaps are `cmsgpack`, `struct`, `setfenv`/`getfenv`, `newproxy`, and the rarely-used `__mode`/`__len`/`__gc` metamethods; see Limitations.

### Can I replicate from / to vendored Redis?

Yes, both directions. The integration tests prove vendored `redis-server` → FrankenRedis replica (with and without `AUTH`) and FrankenRedis → FrankenRedis replica → downstream replica chaining.

### How does the strict/hardened mode actually differ?

Strict mode reproduces upstream behavior byte-for-byte, including upstream quirks (e.g. the `XADD LIMIT 0` wording oddity, the `CLUSTER FAILOVER` gate ordering, the exact `WRONGTYPE` phrasing). Hardened mode preserves the API contract (the same reply *shape*) but adds fail-closed guards: oversized bulks, malformed AOF tails, unknown RESP3 types, and hostile inline strings all get rejected and threat-logged instead of silently bounded-defended.

### Why is the binary called `frankenredis` and not `frankenredis-server`?

A workspace decision in `crates/fr-server/Cargo.toml`: the crate is `fr-server`, the binary is `frankenredis`. Saves typing and reads naturally next to `redis-cli`.

### Where do the names come from?

`frankenlibc` and `frankenfs` are sister projects that share the strict-vs-hardened doctrine. The "Franken-" prefix flags clean-room reimplementations whose first virtue is parity rather than novelty.

### Is there a Sentinel binary?

Not yet. The `fr-sentinel` crate currently exposes the state machine, command surface, and parsers, all unit/fuzz-tested, and the `SENTINEL` command set is wired through the main runtime via the `Store::sentinel_mode` flag. The missing piece is a dedicated `fr-sentinel` launcher binary mirroring `redis-sentinel`, which would take a sentinel config file and enter sentinel mode at startup.

### How is performance trending?

Single-command throughput sits at **71–83% of vendored Redis 7.2.4** on the standard `fr-bench` workloads (geometric mean ~77%), with sub-millisecond p50 latency. Pipelined throughput (`pipeline=16`) sits at ~33–47% of Redis; the bottleneck is the lack of `writev` scatter-gather on the write path, and closing it is the next perf workstream. Each optimization round produces an `ISOMORPHISM_PROOF_*.md` artifact next to before/after flamegraphs under `artifacts/optimization/` so nothing changes observable behavior to gain a few microseconds.

### Where does the parity bar come from?

A vendored Redis 7.2.4 source tree under `legacy_redis_code/redis/`, fully built and used as the live oracle by the conformance harness. When you see `(frankenredis-<slug>)` in a commit message, that's a beads issue ID, usually filed by a differential probe sweep that found a divergence against this oracle.

### What does the `CHANGELOG.md` look like?

Organized by date-bounded development phases (Phase 1–11) from foundation through current state, with thematic capability sections, live-linked representative commits, and a version timeline table. There are no tagged releases; the changelog is the version spine.

---

## Repository layout

```
frankenredis/
├── AGENTS.md                                    # multi-agent build doctrine
├── CHANGELOG.md                                 # 11 phases, 2,354 commits across 78 days
├── COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md    # design spec
├── FEATURE_PARITY.md                            # per-feature parity matrix
├── PLAN_TO_PORT_REDIS_TO_RUST.md                # porting plan
├── SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md   # threat model
├── SENTINEL_SPEC.md                             # sentinel design extract
├── TEST_LOG_SCHEMA_V1.md                        # structured test log contract
├── README.md                                    # (this file)
│
├── Cargo.toml                                   # workspace root
├── rust-toolchain.toml                          # nightly pin
│
├── crates/
│   ├── fr-protocol/                             # RESP2/3 parser + encoder
│   ├── fr-command/                              # dispatch + Lua 5.1 evaluator
│   ├── fr-store/                                # data engine
│   ├── fr-expire/                               # TTL evaluation
│   ├── fr-persist/                              # AOF + RDB + listpack
│   ├── fr-repl/                                 # replication FSM
│   ├── fr-config/                               # strict/hardened policy, TLS, encoding
│   ├── fr-runtime/                              # Runtime orchestrator
│   ├── fr-eventloop/                            # deterministic event-loop planning
│   ├── fr-server/                               # `frankenredis` binary
│   ├── fr-bench/                                # TCP benchmark harness
│   ├── fr-conformance/                          # differential conformance + orchestrators
│   └── fr-sentinel/                             # Sentinel reimplementation
│
├── legacy_redis_code/redis/                     # vendored Redis 7.2.4 (oracle)
├── baselines/                                   # checked-in baseline JSON
├── artifacts/                                   # optimization proofs, schema gates, durability notes
├── fuzz/                                        # 33 cargo-fuzz targets
├── scripts/                                     # baseline / gate / triage scripts
├── docs/                                        # THREAT_MODEL.md
└── .github/workflows/live-conformance-gates.yml # CI: live differential conformance
```

---

## Validation commands

The set of commands a contributor runs before sending a PR. If `rch` is available, all of these can be transparently offloaded to remote workers; otherwise they run locally.

```bash
# Formatting + lints + workspace check
rch exec -- cargo fmt --check
rch exec -- cargo check  --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings

# Unit + conformance tests
rch exec -- cargo test --workspace
rch exec -- cargo test -p fr-conformance -- --nocapture

# Phase 2c schema readiness gate (extraction packets)
rch exec -- cargo run -p fr-conformance --bin phase2c_schema_gate -- --optimization-gate

# Benchmarks
rch exec -- cargo bench
./scripts/record_baselines.sh                    # writes baselines/
./scripts/benchmark_gate.sh                      # gates a candidate run
```

---

## Key documents

- [`AGENTS.md`](AGENTS.md) — multi-agent build/coordination conventions
- [`CHANGELOG.md`](CHANGELOG.md) — date-bounded phase history
- [`FEATURE_PARITY.md`](FEATURE_PARITY.md) — per-feature parity matrix
- [`COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md`](COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md) — design spec
- [`SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md`](SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md) — threat-class matrix
- [`SENTINEL_SPEC.md`](SENTINEL_SPEC.md) — Sentinel design extract
- [`TEST_LOG_SCHEMA_V1.md`](TEST_LOG_SCHEMA_V1.md) — structured test log contract
- [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md) — threat model
- [`artifacts/optimization/phase2-final/DELTA_REPORT.md`](artifacts/optimization/phase2-final/DELTA_REPORT.md) — current performance delta
- [`artifacts/optimization/throughput-gap/ISOMORPHISM_PROOF_LAZY_DIGEST.md`](artifacts/optimization/throughput-gap/ISOMORPHISM_PROOF_LAZY_DIGEST.md) — throughput-recovery proof

---

## References and prior art

Pointers to the literature and to related projects whose decisions FrankenRedis either inherits, contrasts with, or learned from.

### Redis itself

- **Redis source tree.** [`redis/redis`](https://github.com/redis/redis). The 7.2.4 source is vendored under `legacy_redis_code/redis/` as the live conformance oracle. Every behavioral question in this project terminates in "what does this file say?".
- **Redis Streams design note.** Antirez (Salvatore Sanfilippo), *"Streams: A new general purpose data structure in Redis"* (2017). The radix-tree-of-listpacks layout and the consumer-group model originated here.
- **Salvatore Sanfilippo, *"Redis Latency Diagnosis"*.** The documented methodology that informs the `LATENCY` / `SLOWLOG` surface and the latency-monitor budget.

### Algorithms

- **Skiplist.** William Pugh, *"Skip Lists: A Probabilistic Alternative to Balanced Trees"* (CACM 1990). FrankenRedis uses a `BTreeMap` instead, but the observable contract (`O(log n)` ordered access, by-rank lookup, by-range scan) is the same.
- **HyperLogLog.** Flajolet, Fusy, Gandouet, Meunier, *"HyperLogLog: the analysis of a near-optimal cardinality estimation algorithm"* (2007); plus Heule, Nunkesser, Hall, *"HyperLogLog in Practice"* (2013) for the small-cardinality bias correction and the sparse representation.
- **Geohash.** G. M. Morton, *"A computer-oriented geodetic data base and a new technique in file sequencing"* (IBM 1966); the Z-order curve / bit-interleaving idea Redis applies to spatial indexing.
- **CRC64.** Jones polynomial as adapted in `redis/src/crc64.c`; the Reed-Solomon-style polynomial-arithmetic background is covered in Koopman & Chakravarty, *"32-Bit Cyclic Redundancy Codes for Internet Applications"* (DSN 2002).
- **LZF compression.** Marc Lehmann's pure-stream variant (LibLZF). The format predates LZ4 and is what RDB uses on disk.
- **RaptorQ.** RFC 6330, *"RaptorQ Forward Error Correction Scheme for Object Delivery"* (Luby et al., 2011). Roadmap item for the durability sidecar.

### Comparable clean-room reimplementations

- **KeyDB.** Multithreaded Redis fork in C++. Solved scale-up via per-shard threads; FrankenRedis keeps the single-threaded model and goes after memory safety + parity instead.
- **Dragonfly.** C++ shared-nothing multi-threaded Redis-compatible. Re-architected the data layer for multi-core throughput; not a parity project.
- **Microsoft Garnet.** .NET RESP-compatible server with very high throughput. Different language / runtime trade-offs.
- **Valkey.** Linux Foundation Redis fork. Closest to upstream by lineage; FrankenRedis differs by being a clean-room rewrite in Rust.
- **TiKV / FoundationDB.** Distributed KV stores, *not* Redis-compatible. Cited here because they are the canonical examples of "Rust / C++ memory-safe rewrites of a database protocol" and inform some of the testing-discipline choices (differential testing, deterministic simulation, lineage marker artifacts).

### Doctrine

- **`frankenlibc` / `frankenfs`.** Sibling projects in this author's portfolio that share the strict/hardened mode split and the threat-event ledger pattern.
- **`beads_rust` (`br`).** Local-first issue tracker used for parity bookkeeping. Every `(frankenredis-<slug>)` commit tag references a tracked beads issue.
- **Differential testing.** Yang et al., *"Finding and Understanding Bugs in C Compilers"* (PLDI 2011, *Csmith*), the canonical reference for differential testing as a methodology. The FrankenRedis probe-sweep workflow is the same idea applied to a stateful protocol server.

---

## Repository statistics

Numbers behind the "clean-room reimplementation" claim:

| What | Count |
|---|---|
| Crates in the workspace | 13 |
| Rust source files (excluding tests) | 40 |
| Rust source lines (excluding tests, fuzz harnesses, conformance fixtures) | ~186,000 |
| Lines in `fr-command/src/lib.rs` (largest single file — dispatch + 231 command arms) | ~67,600 |
| Lines in `fr-command/src/lua_eval.rs` (custom Lua 5.1 evaluator) | ~18,000 |
| Lines in `fr-runtime/src/lib.rs` (Runtime orchestrator) | ~28,500 |
| Lines in `fr-store/src/lib.rs` (data engine) | ~24,600 |
| Lines in `fr-conformance/src/lib.rs` (test harness) | ~13,300 |
| Lines in `fr-persist/src/lib.rs` (AOF + RDB) | ~6,600 |
| Lines in `fr-server/src/main.rs` (mio TCP server) | ~6,100 |
| Lines in `fr-protocol/src/lib.rs` (RESP codec) | ~2,100 |
| Commits on `main` | 2,354 |
| Active development days | 78 |
| Conformance fixture cases | 4,975 |
| Conformance fixture families | 43 |
| `cargo-fuzz` targets | 33 |
| Redis 7.2.4 base commands implemented | 241 |
| Distinct command-name dispatch arms in `fr-command` | 231 |
| RDB version emitted | 11 |
| Open parity beads as of 2026-05-17 | 14 (1 P2, 2 P3, 11 P4) |
| `unsafe` blocks across all 13 crates | 3 (all `libc::waitpid` in `fr-runtime`) |
| Tagged releases | 0 (pre-1.0; `main` is the version spine) |

---

## Glossary

| Term | Meaning |
|---|---|
| **AOF** | Append-Only File. A RESP-encoded log of every write command, replayed on startup to reconstruct the in-memory store. |
| **beads** | Local-first issue tracker (the `br` CLI from [`beads_rust`](https://github.com/Dicklesworthstone/beads_rust)) used to file and track parity gaps. Tags like `(frankenredis-xyz)` in commits reference beads issue IDs. |
| **BGSAVE / BGREWRITEAOF** | Background snapshot / AOF rewrite, executed in a forked child process so the live server keeps serving traffic during the rewrite. |
| **CRC64** | The 64-bit cyclic-redundancy-check polynomial Redis uses to fingerprint RDB files and `DUMP`/`RESTORE` payloads. Uses the Redis-specific polynomial. |
| **DLRC** | Deterministic Latency Replication Core. The project's tagline for "strict command semantics + tail-aware scheduling + recoverable persistence." |
| **DUMP / RESTORE** | Single-key serialization commands. `DUMP` returns the RDB-encoded value + CRC64; `RESTORE` injects it into another instance. The on-wire format is byte-compatible with vendored Redis 7.2.4. |
| **embstr** | "Embedded string": a Redis encoding for strings ≤44 bytes where the SDS header lives in the same allocation as the object header. FrankenRedis reports this via `OBJECT ENCODING`. |
| **FCALL / FCALL_RO** | Function call. Invokes a function loaded by `FUNCTION LOAD`. The read-only variant rejects writes. |
| **FrankenRedis modes** | `Mode::Strict` (byte-exact upstream parity) or `Mode::Hardened` (preserves contract, adds fail-closed guards). Set via `--mode`. |
| **FULLRESYNC** | The PSYNC response that triggers a full snapshot stream from primary to replica when partial resync isn't possible. |
| **HdrHistogram** | "High dynamic range histogram": a fixed-memory data structure with bounded relative precision used to record latency distributions. |
| **intset** | Redis encoding for sets containing only integers. Stored as a sorted array of fixed-width integers. |
| **listpack** | Redis 7.x replacement for ziplist. A compact, immutable-on-mutation sequential layout used for small hashes, sets, sorted sets, and stream entries. |
| **LZF** | Marc Lehmann's lightweight compression algorithm; used by Redis (and FrankenRedis) for RDB string compression. |
| **mio** | `mio` is the Rust non-blocking I/O abstraction over `epoll`/`kqueue`. fr-server's event loop is built on it. |
| **PSYNC** | Replication handshake command that asks for partial sync if `(repl_id, offset)` is in the primary's backlog window, falling back to FULLRESYNC otherwise. |
| **quicklist** | Redis list encoding: a doubly-linked list of listpack nodes. The compromise between linked-list flexibility and contiguous-memory cache friendliness. |
| **RaptorQ** | A systematic fountain code (RFC 6330) for forward error correction; named in AGENTS.md as the planned mechanism for durability sidecars on long-lived artifacts. Not yet implemented. |
| **RDB** | Redis Database. A binary point-in-time snapshot of the keyspace. FrankenRedis emits RDB version 11. |
| **RESP / RESP2 / RESP3** | REdis Serialization Protocol. RESP2 is the legacy line/bulk/array/integer/error protocol; RESP3 adds map/set/double/bool/null/big-number types. Negotiated via `HELLO`. |
| **S_DOWN / O_DOWN** | Sentinel subjective-down / objective-down. Subjective = "this Sentinel hasn't heard from the instance in `down-after-milliseconds`"; objective = "at least `quorum` Sentinels agree." |
| **SDS** | Simple Dynamic String. Redis's string type. FrankenRedis stores the equivalent as `Vec<u8>` with promotion metadata on the wrapping `Entry`. |
| **skiplist** | Probabilistic ordered data structure Redis uses for the ordered side of large sorted sets. FrankenRedis uses a `BTreeMap` instead with equivalent observable complexity. |
| **threat class** | One of eight categories in `fr-config::ThreatClass` describing what kind of misbehavior a request represents. Drives the policy decision in hardened mode. |
| **vendored Redis** | A full upstream Redis 7.2.4 source tree checked into `legacy_redis_code/redis/`, built and used as the live oracle by the conformance harness. |
| **ziplist** | Legacy Redis compact encoding for small collections. Replaced by listpack in Redis 7.x and not emitted by FrankenRedis. |

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

MIT License (with OpenAI/Anthropic Rider). See [`LICENSE`](LICENSE).
