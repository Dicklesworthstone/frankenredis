# FR-P2C-006 Legacy Anchor Map

Packet: `FR-P2C-006`  
Subsystem: replication state machine  
Target crates: `crates/fr-repl`, `crates/fr-runtime`, `crates/fr-command`, `crates/fr-conformance`  
Prepared by: `PeachKnoll`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact extracts line-anchored replication-state-machine contracts from
legacy Redis and maps them to current FrankenRedis coverage, with normal/edge/
adversarial behavior rows and deterministic failure reason codes for downstream
verification.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-006-A01` | `legacy_redis_code/redis/src/replication.c:244-357` | Replication backlog lifecycle defines offset origin, index strategy, and replid reset on fresh backlog creation. | Missing backlog lifecycle implementation. |
| `FR-P2C-006-A02` | `legacy_redis_code/redis/src/replication.c:588-682` | Master command propagation serializes RESP stream deterministically, including `SELECT` boundaries and repl offset advancement. | Missing command->replication stream serializer. |
| `FR-P2C-006-A03` | `legacy_redis_code/redis/src/replication.c:723-739` | Replica-of-master mode proxies exact applied stream to sub-replicas/backlog without re-encoding divergence. | Missing stream proxy path. |
| `FR-P2C-006-A04` | `legacy_redis_code/redis/src/replication.c:883-936` | Full-resync setup sets slave state, emits `+FULLRESYNC`, and aligns downstream command stream start semantics. | Missing full-resync response/state orchestration. |
| `FR-P2C-006-A05` | `legacy_redis_code/redis/src/replication.c:943-1039` | Master-side PSYNC acceptance requires replid/history compatibility and backlog coverage; success emits `+CONTINUE`, else full-resync path. | Missing master-side PSYNC contract. |
| `FR-P2C-006-A06` | `legacy_redis_code/redis/src/replication.c:1144-1278` | `SYNC/PSYNC` entrypoint enforces failover/master-link constraints, partial-resync attempt, and fallback full sync state setup. | Missing `SYNC/PSYNC` command surface. |
| `FR-P2C-006-A07` | `legacy_redis_code/redis/src/replication.c:1364-1570` | `REPLCONF` option contract covers capabilities, ACK/FACK offsets, GETACK, and rdb-channel negotiation semantics. | Missing `REPLCONF` command/ack path. |
| `FR-P2C-006-A08` | `legacy_redis_code/redis/src/replication.c:1697-1990` | RDB transfer pipeline (`sendBulkToSlave`) and online transition are coupled to ACK-driven stream enable semantics. | Missing RDB bulk transfer + online transition state machine. |
| `FR-P2C-006-A09` | `legacy_redis_code/redis/src/replication.c:2017-2047` | Replid evolution (`change`, `clear`, `shift`) preserves partial-resync history windows across role changes. | Missing replid history model. |
| `FR-P2C-006-A10` | `legacy_redis_code/redis/src/replication.c:2187-2238` | On full dataset replacement, cached master/backlog behavior and bulk-payload intake path must force chained replica resync safety. | Missing attach-to-new-master/full-load transition policy. |
| `FR-P2C-006-A11` | `legacy_redis_code/redis/src/replication.c:2816-3002` | Replica-side PSYNC parser handles `+FULLRESYNC`, `+CONTINUE`, `+RDBCHANNELSYNC`, transient errors, and retry/fallback policy. | Missing replica-side PSYNC negotiation state machine. |
| `FR-P2C-006-A12` | `legacy_redis_code/redis/src/replication.c:3007-3296` | `syncWithMaster` handshake sequence is strict: PING, optional AUTH, REPLCONF phases, PSYNC send/receive, and SYNC fallback. | Missing handshake sequencer. |
| `FR-P2C-006-A13` | `legacy_redis_code/redis/src/replication.c:3450-3500` | `replicationSetMaster` transitions node to replica mode while preserving cached-master path for potential PSYNC reuse. | Missing role-switch-to-replica transition logic. |
| `FR-P2C-006-A14` | `legacy_redis_code/redis/src/replication.c:3503-3562` | `replicationUnsetMaster` turns node into master, shifts replid history, disconnects replicas, and restarts AOF when needed. | Missing role-switch-to-master transition logic. |
| `FR-P2C-006-A15` | `legacy_redis_code/redis/src/replication.c:3566-3598` | Master disconnection path updates replication state counters and immediate reconnect strategy while deferring chained replica disconnect until needed. | Missing disconnection/reconnect policy path. |
| `FR-P2C-006-A16` | `legacy_redis_code/redis/src/replication.c:4284-4343` | `REPLICAOF` command enforces cluster/failover constraints and idempotent/no-op semantics for already-matching master. | Missing `REPLICAOF` surface. |
| `FR-P2C-006-A17` | `legacy_redis_code/redis/src/replication.c:4348-4403` | `ROLE` response format exposes role-dependent replication metadata and replica list offsets. | Missing `ROLE` contract surface. |
| `FR-P2C-006-A18` | `legacy_redis_code/redis/src/replication.c:4409-4427` | Replica ACK emission (`REPLCONF ACK` with optional `FACK`) reflects processed/fsynced offsets. | Missing ACK emission path. |
| `FR-P2C-006-A19` | `legacy_redis_code/redis/src/replication.c:4654-4760` | `WAIT`/`WAITAOF` rely on offset-threshold checks and block/unblock semantics tied to replica/local fsync acknowledgments. | Missing WAIT/WAITAOF semantics. |
| `FR-P2C-006-A20` | `legacy_redis_code/redis/src/replication.c:4773-4845` | Blocked WAIT clients are unblocked only when per-request offset and local/replica thresholds are satisfied. | Missing deterministic wait-unblock reducer. |
| `FR-P2C-006-A21` | `legacy_redis_code/redis/src/replication.c:4871-5035` | `replicationCron` enforces timeout handling, periodic keepalive/newline behavior, and backlog TTL/replid rotation policy. | Missing replication cron policy loop. |
| `FR-P2C-006-A22` | `legacy_redis_code/redis/src/server.c:1922-1978` | `beforeSleep` batches GETACK broadcast and updates fsynced replication offset to promptly wake WAIT/WAITAOF clients. | Missing event-loop replication wake integration. |
| `FR-P2C-006-A23` | `legacy_redis_code/redis/src/server.c:3999-4098` | Command execution propagation sets replication targets and records `c->woff` from `master_repl_offset` after propagation. | Missing woff tracking contract. |
| `FR-P2C-006-A24` | `legacy_redis_code/redis/src/server.c:7410-7464` | RDB load path restores replid/offset metadata and rebases backlog/index offsets depending on master/replica role. | Missing load-time repl metadata rebase logic. |
| `FR-P2C-006-A25` | `legacy_redis_code/redis/src/networking.c:3323-3352` | `commandProcessed` updates master client applied offset and proxies only applied stream delta to backlog/sub-replicas. | Missing applied-delta proxy path. |
| `FR-P2C-006-A26` | `crates/fr-repl/src/lib.rs:3-54`, `crates/fr-command/src/lib.rs:47-137`, `crates/fr-runtime/src/lib.rs:98-227` | Rust baseline has only minimal `ReplProgress` state/offset counters and no PSYNC/REPLCONF/WAIT/ROLE integration. | Present as skeletal replication substrate only. |

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-006-B01` | Normal | Master receives PSYNC with compatible replid and covered offset | Partial resync accepted with `+CONTINUE`; backlog delta streamed from requested offset. | `FR-P2C-006-U001` | `FR-P2C-006-E001` | `repl.psync_partial_accept_mismatch` |
| `FR-P2C-006-B02` | Normal | Master cannot satisfy PSYNC due to history/backlog constraints | Full resync path selected with deterministic state transitions and `+FULLRESYNC` contract. | `FR-P2C-006-U002` | `FR-P2C-006-E002` | `repl.psync_fullresync_fallback_mismatch` |
| `FR-P2C-006-B03` | Normal | Replica handshake (`syncWithMaster`) successful | Ordered sequence `PING -> AUTH? -> REPLCONF -> PSYNC` transitions to connected replication state. | `FR-P2C-006-U003` | `FR-P2C-006-E003` | `repl.handshake_state_machine_mismatch` |
| `FR-P2C-006-B04` | Normal | `REPLCONF ACK/FACK` updates arrive | Replica ack offsets/time refresh and command-stream enable behavior follow ack contract. | `FR-P2C-006-U004` | `FR-P2C-006-E004` | `repl.replconf_ack_offset_contract_violation` |
| `FR-P2C-006-B05` | Normal | `WAIT` request with reachable offsets | Immediate or blocked reply count matches replicas acknowledged at/above `c->woff`. | `FR-P2C-006-U005` | `FR-P2C-006-E005` | `repl.wait_ack_count_mismatch` |
| `FR-P2C-006-B06` | Normal | `WAITAOF` request with local + replica constraints | Reply tuple `[acklocal, ackreplicas]` reflects fsynced local and replica AOF-ack thresholds. | `FR-P2C-006-U006` | `FR-P2C-006-E006` | `repl.waitaof_ack_semantics_mismatch` |
| `FR-P2C-006-B07` | Edge | `REPLICAOF NO ONE` on current replica | Role transition to master shifts replid history and disconnects slaves for resync awareness. | `FR-P2C-006-U007` | `FR-P2C-006-E007` | `repl.role_transition_replid_shift_mismatch` |
| `FR-P2C-006-B08` | Edge | Master disconnect while replica mode active | Reconnect state/path engaged without premature chained-replica disconnect until full-resync need is known. | `FR-P2C-006-U008` | `FR-P2C-006-E008` | `repl.master_disconnect_reconnect_policy_violation` |
| `FR-P2C-006-B09` | Adversarial | PSYNC replid mismatch / out-of-range requested offset | Partial resync is rejected deterministically, preventing divergent history continuation. | `FR-P2C-006-U009` | `FR-P2C-006-E009` | `repl.psync_replid_or_offset_reject_mismatch` |
| `FR-P2C-006-B10` | Adversarial | Replica receives malformed `+FULLRESYNC` response syntax | PSYNC parser fails closed and avoids adopting invalid replication metadata. | `FR-P2C-006-U010` | `FR-P2C-006-E010` | `repl.fullresync_reply_parse_violation` |
| `FR-P2C-006-B11` | Adversarial | Backlog idle timeout reached on master without slaves | Backlog is freed and replid history reset to prevent unsafe future partial-resync acceptance. | `FR-P2C-006-U011` | `FR-P2C-006-E011` | `repl.backlog_ttl_replid_reset_violation` |
| `FR-P2C-006-B12` | Adversarial | Hardened mode faces non-allowlisted replication deviation | Non-allowlisted behavior is rejected and packet remains strict-equivalent fail-closed. | `FR-P2C-006-U012` | `FR-P2C-006-E012` | `repl.hardened_nonallowlisted_rejected` |

## High-risk traceability and structured-log contract

For all `FR-P2C-006-U*` and `FR-P2C-006-E*` rows, logs must include:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-006`)
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

- Unit/property: `fr_repl_phase2c_packet_006`
- E2E/integration: `fr_runtime_phase2c_packet_006`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-006-CLAIM-01` |
| `evidence_id` | `FR-P2C-006-EVID-LEGACY-REPL-001` |
| Hotspot evidence | `A05`, `A11`, `A19` (PSYNC negotiation + wait-ack semantics) |
| Mapped graveyard section IDs | `AG-DET-04` (deterministic state reduction), `AG-NET-06` (handshake resilience), `AG-SEC-11` (fail-closed policy) |
| Baseline comparator | Legacy Redis replication state machine (`replication.c` + `server.c` + `networking.c`) |
| EV score | `2.9` |
| Priority tier | `S` |
| Adoption wedge | Implement PSYNC+ACK core first, then WAIT/WAITAOF and role-transition boundaries |
| Budgeted mode defaults | Strict: `FailClosed`; Hardened: bounded defenses only on allowlist |
| Deterministic exhaustion behavior | On hardened budget exhaustion, force strict-equivalent fail-closed and emit `repl.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-repl -- --nocapture FR_P2C_006`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_006` |

## Expected-loss decision model

States:

- `S0`: contract-preserving replication path
- `S1`: recoverable bounded condition (allowlisted diagnostics/clamp)
- `S2`: unsafe replication-order or history-divergence condition

Actions:

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense with evidence emission
- `A2`: fail closed and reject/abort transition

Loss matrix (lower is better):

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)` rises with transient handshake/parser anomalies without history drift.
- `P(S2|e)` rises with replid mismatch, invalid offset acceptance, or wait-ack contract divergence.

Calibration and fallback policy:

- Calibration metric: false-negative rate on adversarial replication suite `< 1%`.
- Fallback trigger: calibration breach in two consecutive windows or `P(S2|e) >= 0.30`.
- Trigger behavior: disable hardened deviations for packet scope and enforce strict fail-closed.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever (downstream execution target):

- `LEV-006-01`: deterministic PSYNC/WAIT decision cache keyed by `(replid_epoch, offset_window, ack_snapshot_epoch)` with strict invalidation on role/backlog transitions.

Required loop artifacts and paths:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-006/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-006/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-006/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-006/isomorphism_report.md`

Replay commands (strict/hardened):

- `rch exec -- cargo test -p fr-repl -- --nocapture FR_P2C_006_STRICT`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_006_HARDENED`

## Reproducibility and provenance pack references

Required pack (to be produced and linked by downstream packet beads):

- `artifacts/phase2c/FR-P2C-006/env.json`
- `artifacts/phase2c/FR-P2C-006/manifest.json`
- `artifacts/phase2c/FR-P2C-006/repro.lock`
- `artifacts/phase2c/FR-P2C-006/LEGAL.md` (mandatory if IP/provenance risk is detected)

## Confidence notes

- High confidence for replication state-machine anchors and transition constraints (direct extraction from `replication.c`, `server.c`, `networking.c`).
- Medium confidence for eventual Rust module split and optimization-lever details pending downstream implementation beads.
