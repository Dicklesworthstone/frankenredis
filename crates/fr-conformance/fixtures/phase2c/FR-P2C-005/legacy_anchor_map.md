# FR-P2C-005 Legacy Anchor Map

Packet: `FR-P2C-005`  
Subsystem: persistence format and replay  
Target crates: `crates/fr-persist`, `crates/fr-runtime`, `crates/fr-command`, `crates/fr-store`  
Prepared by: `PeachKnoll`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact extracts line-anchored AOF/RDB persistence and replay contracts from
legacy Redis and maps them to current FrankenRedis coverage, with explicit
normal/edge/adversarial behavior rows for downstream verification.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-005-A01` | `legacy_redis_code/redis/src/aof.c:231-261` | Startup loads AOF manifest from disk; missing directory/file is tolerated for upgrade path, malformed manifest is not. | Missing manifest subsystem in `fr-persist`. |
| `FR-P2C-005-A02` | `legacy_redis_code/redis/src/aof.c:263-360` | Manifest parser enforces strict line schema, filename basename-only policy, and single BASE identity. | Missing. |
| `FR-P2C-005-A03` | `legacy_redis_code/redis/src/aof.c:727-782` | On boot with AOF enabled: create base when dataset empty, open/create INCR with `O_APPEND`, then persist manifest synchronously or abort. | Missing open/manifest orchestration. |
| `FR-P2C-005-A04` | `legacy_redis_code/redis/src/aof.c:804-868` | Opening a new INCR AOF is atomic with rollback semantics across fd + manifest update. | Missing atomic rotate in `fr-persist`. |
| `FR-P2C-005-A05` | `legacy_redis_code/redis/src/aof.c:885-912` | Replay offset restoration uses INCR `end_offset` when present; otherwise computes from `start_offset + file_size`. | Missing repl-offset recovery in persistence crate. |
| `FR-P2C-005-A06` | `legacy_redis_code/redis/src/aof.c:1409-1448` | AOF feed must emit optional timestamp annotations, `SELECT` on DB boundary changes, then append canonical command serialization. | Partial: `AofRecord` encodes RESP argv only (`crates/fr-persist/src/lib.rs:15-40`). |
| `FR-P2C-005-A07` | `legacy_redis_code/redis/src/server.c:3594-3608`, `legacy_redis_code/redis/src/server.c:3999-4035` | Command propagation to AOF follows post-command dirty/flag logic; selective suppression/forcing must be respected. | Missing server-side propagation policy in Rust runtime. |
| `FR-P2C-005-A08` | `legacy_redis_code/redis/src/server.c:1857-1883`, `legacy_redis_code/redis/src/server.c:1951-1979` | `beforeSleep` flushes AOF before pending writes and updates `fsynced_reploff` visibility for `WAITAOF`. | Missing event-loop-integrated persistence flush path. |
| `FR-P2C-005-A09` | `legacy_redis_code/redis/src/aof.c:1147-1260` | Flush path handles delayed fsync, bounded postponement, partial/short write error accounting, and durability telemetry. | Missing flush/fsync policy surface. |
| `FR-P2C-005-A10` | `legacy_redis_code/redis/src/aof.c:1507-1577` | AOF replay accepts either pure RESP log or RDB preamble + AOF tail, with fake AOF client execution context. | Missing combined preamble/tail loader. |
| `FR-P2C-005-A11` | `legacy_redis_code/redis/src/aof.c:1580-1699` | Replay parser enforces `*<argc>`/`$<len>` framing, command lookup, and executes commands in strict serialized order. | Partial: RESP parsing exists in `fr-protocol`; no replay loop in `fr-persist`. |
| `FR-P2C-005-A12` | `legacy_redis_code/redis/src/aof.c:1660-1711` | MULTI replay invariant: if EOF arrives mid-transaction, roll back to `valid_before_multi` and treat as truncation path. | Missing transaction-aware replay rollback. |
| `FR-P2C-005-A13` | `legacy_redis_code/redis/src/aof.c:1725-1759` | Recovery policy gates: truncated/corrupt tail can be auto-trimmed only under explicit config bounds; otherwise hard failure. | Missing bounded replay-repair policy hooks. |
| `FR-P2C-005-A14` | `legacy_redis_code/redis/src/aof.c:1775-1897` | Manifest-driven replay ordering: BASE then each INCR; truncation/corruption is only tolerated on final file. | Missing ordered multi-file replay engine. |
| `FR-P2C-005-A15` | `legacy_redis_code/redis/src/aof.c:2524-2577` | Rewrite output is atomic (`temp` + `rename`) and can use RDB preamble as optimization without changing replay semantics. | Missing rewrite pipeline in Rust. |
| `FR-P2C-005-A16` | `legacy_redis_code/redis/src/aof.c:2604-2670` | `BGREWRITEAOF` parent/child protocol rotates INCR first, forks child base rewrite, then commits on child success. | Missing background rewrite state machine. |
| `FR-P2C-005-A17` | `legacy_redis_code/redis/src/rdb.c:1673-1708` | RDB save stream contract: header, AUX/meta, functions, DB payloads, EOF opcode, checksum trailer. | Missing RDB serializer in `fr-persist`. |
| `FR-P2C-005-A18` | `legacy_redis_code/redis/src/rdb.c:1816-1856` | On-disk RDB save must be atomic via temp file + rename + directory fsync. | Missing RDB atomic save implementation. |
| `FR-P2C-005-A19` | `legacy_redis_code/redis/src/rdb.c:3651-3833` | RDB load contract is opcode-driven (`SELECTDB`, `RESIZEDB`, expiry, AUX, module aux) and must ignore unknown AUX fields safely. | Missing RDB loader with opcode handling. |
| `FR-P2C-005-A20` | `legacy_redis_code/redis/src/rdb.c:3917-4042` | Load-time expiry semantics are role-sensitive; master may expire stale keys when not loading AOF preamble; duplicate keys panic unless allow-dup mode. | Missing role-aware replay/load semantics. |
| `FR-P2C-005-A21` | `legacy_redis_code/redis/src/rdb.c:4013-4029` | CRC64 checksum mismatch is fatal when checksum validation enabled. | Missing checksum validation path. |
| `FR-P2C-005-A22` | `legacy_redis_code/redis/src/server.c:7396-7465` | Boot persistence selection is mode-dependent: AOF (if enabled) has priority over RDB load; replication IDs/offsets rebased from loaded metadata. | Missing startup persistence selection and repl metadata rebase. |
| `FR-P2C-005-A23` | `legacy_redis_code/redis/src/replication.c:2377-2393`, `legacy_redis_code/redis/src/replication.c:2646-2652` | Full sync replay ordering requires stopping AOF before loading incoming RDB, then restarting AOF only after successful sync completion. | Missing sync-time persistence state transitions. |
| `FR-P2C-005-A24` | `legacy_redis_code/redis/src/replication.c:4671-4759`, `legacy_redis_code/redis/src/replication.c:4773-4845` | `WAITAOF` contract ties user-visible ack to local `fsynced_reploff` and replica AOF ack offsets, with deterministic block/unblock behavior. | Missing WAITAOF semantics in command/runtime layers. |
| `FR-P2C-005-A25` | `legacy_redis_code/redis/src/server.c:5089-5109` | Disk errors in AOF/RDB pipelines can hard-deny writes (`MISCONF`) depending on persistence state and fsync status. | Missing disk-error gate in Rust runtime. |
| `FR-P2C-005-A26` | `crates/fr-persist/src/lib.rs:5-40`, `crates/fr-runtime/src/lib.rs:14-265`, `crates/fr-command/src/lib.rs:47-137`, `crates/fr-store/src/lib.rs:46-313` | Current Rust baseline has primitive AOF record frame conversion + evidence ledger + command/store execution, but no manifest/rewrite/RDB loader state machine yet. | Present as minimal substrate only. |

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-005-B01` | Normal | Startup with valid BASE+INCR manifest set | Replay executes BASE then each INCR in manifest order; final state is deterministic. | `FR-P2C-005-U001` | `FR-P2C-005-E001` | `persist.replay.manifest_order_mismatch` |
| `FR-P2C-005-B02` | Normal | AOF file begins with `REDIS` preamble | Loader consumes RDB preamble then resumes AOF tail replay without cursor drift. | `FR-P2C-005-U002` | `FR-P2C-005-E002` | `persist.replay.rdb_preamble_tail_desync` |
| `FR-P2C-005-B03` | Normal | DB changes during command propagation | `SELECT` boundaries and propagated commands match replay-visible order. | `FR-P2C-005-U003` | `FR-P2C-005-E003` | `persist.propagation.select_boundary_mismatch` |
| `FR-P2C-005-B04` | Edge | Truncated final INCR AOF file and truncate policy enabled | Loader truncates to last valid offset and returns recoverable success state only for final file. | `FR-P2C-005-U004` | `FR-P2C-005-E004` | `persist.replay.tail_truncate_recover` |
| `FR-P2C-005-B05` | Edge | Truncated non-final file in manifest chain | Replay must fail closed (fatal) and never continue to later files. | `FR-P2C-005-U005` | `FR-P2C-005-E005` | `persist.replay.nonfinal_truncation_fatal` |
| `FR-P2C-005-B06` | Edge | EOF mid `MULTI` transaction during replay | Loader rewinds to `valid_before_multi`; incomplete transaction is not applied. | `FR-P2C-005-U006` | `FR-P2C-005-E006` | `persist.replay.incomplete_multi_rollback` |
| `FR-P2C-005-B07` | Edge | Manifest line malformed / path traversal filename | Loader aborts startup with strict manifest parse error; no partial replay accepted. | `FR-P2C-005-U007` | `FR-P2C-005-E007` | `persist.manifest.parse_or_path_violation` |
| `FR-P2C-005-B08` | Adversarial | Corrupt RDB payload or checksum mismatch | RDB load fails closed; corrupted snapshot never becomes active dataset. | `FR-P2C-005-U008` | `FR-P2C-005-E008` | `persist.rdb.checksum_or_format_invalid` |
| `FR-P2C-005-B09` | Adversarial | Disk write/fsync error while AOF enabled | Write commands become denied with deterministic persistence error semantics. | `FR-P2C-005-U009` | `FR-P2C-005-E009` | `persist.disk_error_write_denied` |
| `FR-P2C-005-B10` | Adversarial | Replica full sync with concurrent persistence activity | AOF subsystem is stopped before inbound RDB load and restarted only after successful sync finalization. | `FR-P2C-005-U010` | `FR-P2C-005-E010` | `persist.replication.sync_aof_state_violation` |
| `FR-P2C-005-B11` | Adversarial | `WAITAOF` with local+replica constraints | Unblock only when both local fsync and replica AOF offset thresholds are satisfied. | `FR-P2C-005-U011` | `FR-P2C-005-E011` | `persist.waitaof_ack_semantics_mismatch` |
| `FR-P2C-005-B12` | Adversarial | Hardened mode bounded replay repair candidate | Only allowlisted bounded replay repair is permitted; strict mode remains fail-closed. | `FR-P2C-005-U012` | `FR-P2C-005-E012` | `persist.hardened_repair_policy_violation` |

## High-risk traceability and structured-log contract

For all `FR-P2C-005-U*` and `FR-P2C-005-E*` rows, logs must include:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-005`)
- `mode` (`strict` or `hardened`)
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

Suggested suite IDs:

- Unit/property: `fr_persist_phase2c_packet_005`
- E2E/integration: `fr_runtime_phase2c_packet_005`

## Sequencing boundary notes

- This artifact covers extraction and behavior mapping only.
- Contract table formalization and strict/hardened invariant encoding move to `bd-2wb.16.2`.
- Risk envelope and implementation sequencing are captured by downstream packet beads.

## Confidence notes

- High confidence for AOF/RDB core replay invariants and persistence ordering (direct source extraction).
- High confidence for boot-time persistence selection and replication coupling (`server.c`, `replication.c`).
- Medium confidence for exact future Rust crate boundaries beyond current minimal persistence substrate.
