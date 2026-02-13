# EXISTING_REDIS_STRUCTURE

## 1. Legacy Oracle

- Root: `/data/projects/frankenredis/legacy_redis_code/redis`
- Upstream: `https://github.com/redis/redis`
- Role: behavioral oracle for reply bytes, side effects, ordering, and replay semantics.

## 2. Subsystem Map

| Subsystem | Legacy Paths | Parity Critical Notes |
|---|---|---|
| Event loop + networking | `src/ae.c`, `src/networking.c`, `src/connection.c` | request lifecycle ordering, short write behavior, backpressure behavior |
| Protocol parser | `src/networking.c`, RESP decode paths | frame boundary handling, malformed frame errors, inline/protocol mode edges |
| Command router | `src/server.c`, command table declarations | arity checks, flags, command deny/allow conditions |
| Data model | `src/db.c`, `src/object.c`, `src/dict.c`, `src/quicklist.c`, `src/listpack.c`, `src/rax.c` | deterministic mutation semantics and keyspace side effects |
| Expiry/eviction | `src/expire.c`, `src/evict.c` | TTL return codes (`-2`, `-1`, positive), active/passive expiry interactions |
| Persistence | `src/aof.c`, `src/rdb.c`, `src/rio.c`, `src/bio.c` | replay ordering, data-loss boundaries, corruption handling |
| Replication | `src/replication.c` | offsets, backlog semantics, PSYNC transitions, ordering on reconnect |
| Security/config | `src/acl.c`, `src/config.c`, `src/tls.c` | fail-closed behavior and configuration compatibility |

## 3. Semantics To Preserve Exactly (V1)

1. RESP parse validity boundaries and error class mapping.
2. Command arity and command error strings for scoped V1 commands.
3. Command side effects and reply shape parity.
4. TTL behavior including deletion on immediate expiry.
5. AOF/RDB replay ordering invariants.
6. Replication offset monotonicity and lag accounting semantics.

## 4. Extraction Completeness Matrix (Current)

| Domain | Status | Notes |
|---|---|---|
| Protocol framing | partial | first parser scaffold in Rust done; malformed corpus still incomplete |
| Command table semantics | partial | bootstrap set implemented (`PING/ECHO/SET/GET/DEL/INCR/EXPIRE/PTTL`) |
| Data structure parity | partial | string path only in runtime slice |
| TTL/expiry edge cases | partial | baseline codes and lazy expiry in place |
| Persistence replay | partial | AOF frame contract scaffolded, full replay not yet landed |
| Replication behavior | partial | state/offset scaffolding only |
| ACL/config semantics | not_started | reserved for M3+ |

## 5. V1 Extraction Boundary

Included for V1:
- protocol framing, command core, keyspace string semantics, TTL core, AOF/RDB baseline, replication baseline.

Deferred from V1:
- sentinel, module API, full-cluster orchestration, full TLS/IO-thread parity, scripting/module surface.

## 6. Conformance Fixture Families (Planned)

1. `protocol_core`: valid and malformed RESP frames.
2. `string_core`: deterministic string command semantics.
3. `ttl_core`: expiry edge cases and return-code parity.
4. `persist_core`: replay ordering and state recovery parity.
5. `repl_core`: offset progression and handshake parity.
6. `security_config_core`: fail-closed compatibility checks.

## 7. Drift Severity Taxonomy

- `critical`: reply bytes / side effects / ordering mismatch.
- `high`: compatibility behavior mismatch without immediate data corruption.
- `medium`: recoverable mismatch with explicit exception.
- `low`: non-contract observability differences only.
