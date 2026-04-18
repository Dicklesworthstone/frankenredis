# FrankenRedis Threat Model

Per the Security Doctrine in AGENTS.md: "Threat model notes for each major subsystem."

## 1. Network / RESP Protocol (fr-protocol, fr-server)

| Threat | Mitigation | Residual Risk |
|--------|-----------|---------------|
| Oversized bulk strings | `max_bulk_len` = 512 MiB (ParserConfig). Rejects with `MultibulkLengthTooLarge`. | None for well-configured limit. |
| Deeply nested arrays | `max_recursion_depth` = 128. Rejects with `RecursionLimitExceeded`. | None. |
| Array bomb (1M+ elements) | `max_array_len` = 1M elements. | Large arrays within limit can still consume memory. |
| Slowloris / partial frame stalling | mio non-blocking I/O with per-tick processing. No per-client read timeout currently. | Slow clients can hold connections indefinitely. Mitigation: add `client-idle-timeout`. |
| Pipeline flooding | Client output buffer limit (256 MiB default). Clients exceeding limit are disconnected. | Memory pressure before disconnect triggers. |
| Query buffer exhaustion | `client-query-buffer-limit` = 1 GiB. | Large but bounded. |
| Malformed RESP frames | Parser returns `Err` for all invalid input. Proptest fuzz coverage (40K random inputs, zero panics). | Covered. |

## 2. Authentication (fr-runtime)

| Threat | Mitigation | Residual Risk |
|--------|-----------|---------------|
| Brute force AUTH | No rate limiting. | Add AUTH attempt rate limiting or backoff. |
| Timing attacks on password comparison | Constant-time XOR-based comparison (no early exit). | Length mismatch returns immediately (reveals password length). Consider padding. |
| Default no-auth | Server starts with no password by default (matches Redis). | Deployment responsibility. Document in security guide. |
| ACL bypass | Per-command ACL enforcement via `is_command_authorized()`. Deny-first precedence. 82 conformance tests. | Covered for implemented surface. |

## 3. Persistence (fr-persist)

| Threat | Mitigation | Residual Risk |
|--------|-----------|---------------|
| RDB corruption (disk error) | CRC64 checksum validation on load. Rejects mismatched checksums. | Covered. |
| RDB poisoning (crafted file) | Size clamping on LZF decompression (prevents OOM). Magic/version/EOF validation. Proptest fuzz coverage (30K random inputs, zero panics). | Covered. |
| AOF truncation | AOF load replays available records. Partial frame at end is silently skipped. | Truncation loses data but does not crash. |
| File permission attacks | No explicit permission checks on persistence files. | Deployment responsibility. |
| Atomic write safety | AOF/RDB writes use atomic temp-file + rename. Parent directory fsync after rename. | Covered (crash-safe). |

## 4. Replication (fr-repl, fr-server)

| Threat | Mitigation | Residual Risk |
|--------|-----------|---------------|
| Rogue replica injection | The normal authentication gate applies to replication commands: requirepass/ACL-protected primaries reject unauthenticated `REPLCONF`, `PSYNC`, and `SYNC`. TCP regression coverage freezes this protected-primary behavior. | Redis-compatible no-auth deployments still accept replica handshakes; production deployments should configure auth and network isolation. |
| PSYNC replay attack | PSYNC offset validation against backlog window. Out-of-range offsets trigger FULLRESYNC. | Covered for offset bounds. |
| Snapshot poisoning | RDB snapshots validated with CRC64 on replica side. | Covered. |
| Replication stream injection | No TLS on replication channel. | Add TLS support for replication connections. |
| Backlog memory exhaustion | `repl-backlog-size` configurable (default 1 MiB via config). | Bounded. |

## 5. Lua Scripting (fr-command/lua_eval)

| Threat | Mitigation | Residual Risk |
|--------|-----------|---------------|
| Infinite loops | Iteration limit: 1,000,000 iterations max across all loop types. | Covered. Time-based limit also configurable via `lua-time-limit`. |
| Memory exhaustion | No per-script memory limit. | Large string concatenation or table construction can exhaust server memory. Consider adding. |
| redis.call abuse | Commands executed through normal dispatch with ACL enforcement. | Covered. |
| Coroutine escape | Coroutines not supported. Stubs return error. | Covered. |
| File system access | No `io`, `os.execute`, `loadfile` exposed. Only `os.clock`. | Covered. |
| pcall error masking | pcall/xpcall implemented. Errors are catchable by scripts. | By design (Lua 5.1 spec). |

## 6. Command-Level Threats (fr-command, fr-store)

| Threat | Mitigation | Residual Risk |
|--------|-----------|---------------|
| Integer overflow (INCR/INCRBY) | Checked arithmetic. Returns error on overflow. | Covered. |
| Memory exhaustion (APPEND/LPUSH) | maxmemory eviction with configurable policies (allkeys-lru, volatile-lru, etc.). | Covered when maxmemory is configured. |
| Key enumeration timing | KEYS/SCAN iterate all keys. No timing normalization. | Information leakage via timing. Low severity for most deployments. |
| SORT with external key patterns | BY/GET patterns can access arbitrary keys. | By design (matches Redis). Mitigate via ACL key patterns. |
| DEBUG command abuse | DEBUG commands available. | Restrict via ACL in production. |

## Priority of Remaining Mitigations

1. **High**: Add AUTH rate limiting / backoff for brute force protection
2. **High**: Add TLS support for replication connections
3. **Medium**: Add per-client idle timeout to prevent slowloris
4. **Medium**: Add per-script memory limit for Lua execution
5. **Low**: Pad password length comparison to prevent length leakage
