# FR-P2C-007 Legacy Anchor Map

Packet: `FR-P2C-007`  
Subsystem: cluster behavior (scoped)  
Target crates: `crates/fr-command`, `crates/fr-runtime`, `crates/fr-eventloop`, `crates/fr-conformance`  
Prepared by: `PeachKnoll`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact extracts line-anchored cluster behavior contracts from legacy
Redis (routing, redirection, slot ownership, state transitions, failover
gates, and cluster migration sidecar signals) and maps them to current
FrankenRedis coverage, including normal/edge/adversarial expectations and
packet-specific deterministic reason codes.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-007-A01` | `legacy_redis_code/redis/src/cluster.c:1004-1121` | `CLUSTER` command router enforces subcommand arity/validation and routes `NODES`, `SLOTS`, `SHARDS`, `INFO`, `KEYSLOT`, and key-slot introspection paths. | Missing cluster command surface in `fr-command`. |
| `FR-P2C-007-A02` | `legacy_redis_code/redis/src/cluster.c:1126-1147` | `extractSlotFromKeysResult` enforces single-slot constraint and emits cross-slot classification (`CLUSTER_CROSSSLOT`). | Missing key-slot extraction/reduction path. |
| `FR-P2C-007-A03` | `legacy_redis_code/redis/src/cluster.c:1183-1435` | `getNodeByQuery` determines slot owner and redirection mode (`NONE`, `MOVED`, `ASK`, `TRYAGAIN`, `CLUSTERDOWN`) using slot state, importing/migrating status, read-only flags, and cluster health. | Missing slot-owner query engine and redirect classifier. |
| `FR-P2C-007-A04` | `legacy_redis_code/redis/src/cluster.c:1444-1471` | `clusterRedirectClient` maps internal redirect codes to exact user-visible errors (`-CROSSSLOT`, `-TRYAGAIN`, `-CLUSTERDOWN`, `-MOVED`, `-ASK`). | Missing deterministic redirect error shaping. |
| `FR-P2C-007-A05` | `legacy_redis_code/redis/src/cluster.c:1485-1542` | `clusterRedirectBlockedClientIfNeeded` prevents indefinite blocking by redirecting blocked clients when slot ownership/health changes. | Missing blocked-client redirect safety bridge. |
| `FR-P2C-007-A06` | `legacy_redis_code/redis/src/cluster.c:1638-1676` | `CLUSTER SLOTS` response contract emits slot ranges with master/replica endpoint tuples and availability filtering. | Missing slot-map reply encoder. |
| `FR-P2C-007-A07` | `legacy_redis_code/redis/src/cluster.c:1683-1701`, `legacy_redis_code/redis/src/cluster.c:2228-2234` | `ASKING`, `READONLY`, and `READWRITE` mutate client mode flags that directly alter redirection/read behavior. | Missing client cluster-mode flag state machine. |
| `FR-P2C-007-A08` | `legacy_redis_code/redis/src/cluster.c:2304-2330` | `verifyClusterConfigWithData` enforces DB0-only assumption and repairs data/config drift via claim-unassigned + delete-unowned slot procedures. | Missing startup cluster data/config reconciliation gate. |
| `FR-P2C-007-A09` | `legacy_redis_code/redis/src/cluster_legacy.c:959-1042` | `clusterInit` creates cluster state, loads/creates node config, sets node identity/network metadata, and initializes failover/secret state. | Missing cluster state bootstrap subsystem. |
| `FR-P2C-007-A10` | `legacy_redis_code/redis/src/cluster_legacy.c:1043-1102` | `clusterInitLast` opens cluster bus listener and bind/port safety checks required for runtime cluster transport. | Missing cluster bus listener initialization path. |
| `FR-P2C-007-A11` | `legacy_redis_code/redis/src/cluster_legacy.c:1789-1812` | `clusterHandleConfigEpochCollision` deterministically resolves epoch collisions by lexicographic node-id rule and rebroadcasts PONG/config save. | Missing config-epoch collision resolver. |
| `FR-P2C-007-A12` | `legacy_redis_code/redis/src/cluster_legacy.c:2778-3530` | `clusterProcessPacket` validates cluster bus packet structure/type/length/version and updates node/failover state-machine paths. | Missing cluster bus packet parser/state reducer. |
| `FR-P2C-007-A13` | `legacy_redis_code/redis/src/cluster_legacy.c:3680-3890` | `clusterSendPing` composes gossip payloads and extension data to preserve liveness/failure-report propagation guarantees. | Missing gossip ping/pong emission loop. |
| `FR-P2C-007-A14` | `legacy_redis_code/redis/src/cluster_legacy.c:3891-3905` | `clusterSendFail` broadcasts failing-node transition to converge failure view. | Missing fail-notification broadcast path. |
| `FR-P2C-007-A15` | `legacy_redis_code/redis/src/cluster_legacy.c:3978-4003` | `clusterPropagatePublish` propagates publish traffic across full cluster or per-shard path (`PUBLISH` vs `PUBLISHSHARD`). | Missing cluster publish propagation contract. |
| `FR-P2C-007-A16` | `legacy_redis_code/redis/src/cluster_legacy.c:4298-4472` | `clusterHandleSlaveFailover` enforces election preconditions, rank-based delay, quorum vote counting, and takeover epoch updates. | Missing cluster failover election state machine. |
| `FR-P2C-007-A17` | `legacy_redis_code/redis/src/cluster_legacy.c:4496-4585` | `clusterHandleSlaveMigration` migrates replicas to orphaned masters using barrier and candidate-selection invariants. | Missing replica-migration policy engine. |
| `FR-P2C-007-A18` | `legacy_redis_code/redis/src/cluster_legacy.c:4641-4670` | `clusterHandleManualFailover` gates failover start on master-offset alignment and schedules immediate failover checks. | Missing manual-failover gate control path. |
| `FR-P2C-007-A19` | `legacy_redis_code/redis/src/cluster_legacy.c:4731-4940` | `clusterCron` handles reconnects, liveness checks, ping cadence, timeout-to-PFAIL transitions, and failover/migration triggers. | Missing cluster cron policy loop. |
| `FR-P2C-007-A20` | `legacy_redis_code/redis/src/cluster_legacy.c:4942-4970` | `clusterBeforeSleep` executes deferred cluster tasks (manual failover, state updates, config save, broadcast) before event-loop sleep. | Missing before-sleep deferred cluster task executor. |
| `FR-P2C-007-A21` | `legacy_redis_code/redis/src/cluster_legacy.c:5071-5095` | `clusterAddSlot`/`clusterDelSlot` maintain slot-owner map, owner-not-claiming bitmap consistency, and per-slot stats reset. | Missing slot ownership mutation core. |
| `FR-P2C-007-A22` | `legacy_redis_code/redis/src/cluster_legacy.c:5151-5261` | `clusterUpdateState` computes `CLUSTER_OK`/`CLUSTER_FAIL` using slot coverage, reachable-master quorum, and rejoin-delay safety. | Missing cluster health/quorum reducer. |
| `FR-P2C-007-A23` | `legacy_redis_code/redis/src/cluster_legacy.c:5262-5288` | `clusterClaimUnassignedSlots` claims orphaned populated slots to reconcile runtime data with cluster config. | Missing unassigned-slot claim/recovery routine. |
| `FR-P2C-007-A24` | `legacy_redis_code/redis/src/cluster_legacy.c:5966-6429` | `clusterCommandSpecial` handles administrative cluster operations (`MEET`, `FLUSHSLOTS`, `ADDSLOTS*`, `SETSLOT*`, `FAILOVER`, epoch controls). | Missing cluster admin command contract surface. |
| `FR-P2C-007-A25` | `legacy_redis_code/redis/src/cluster_legacy.c:6524-6536` | `clusterAllowFailoverCmd`/`clusterPromoteSelfToMaster` enforce command-availability policy and promotion sequencing. | Missing failover-command policy bridge. |
| `FR-P2C-007-A26` | `legacy_redis_code/redis/src/server.c:4428-4449` | Main command path invokes cluster routing/redirection pre-dispatch; transaction behavior is adjusted on redirect rejection. | Missing runtime integration seam for cluster pre-dispatch gate. |
| `FR-P2C-007-A27` | `legacy_redis_code/redis/src/server.c:1694-1701`, `legacy_redis_code/redis/src/server.c:1892-1901` | Server cron and `beforeSleep` call cluster hooks at deterministic cadence and ordering. | Missing eventloop/runtime cluster hook wiring. |
| `FR-P2C-007-A28` | `legacy_redis_code/redis/src/blocked.c:42-45`, `legacy_redis_code/redis/src/blocked.c:184-191` | Blocking-operation subsystem explicitly depends on cluster redirect handling for key-wait operations including WAIT/WAITAOF class behavior. | Missing blocked-op + cluster redirect integration checks. |
| `FR-P2C-007-A29` | `legacy_redis_code/redis/src/cluster_asm.c:478-501`, `legacy_redis_code/redis/src/cluster_asm.c:719-779`, `legacy_redis_code/redis/src/cluster_asm.c:2543-2632`, `legacy_redis_code/redis/src/cluster_asm.c:2859-2900` | ASM syncslots sidecar publishes migration task state, rejects cross-slot propagation during migration, and advances import/migrate tasks via cron/before-sleep events. | Missing ASM-equivalent migration sidecar control plane. |
| `FR-P2C-007-A30` | `crates/fr-command/src/lib.rs:47-133`, `crates/fr-runtime/src/lib.rs:53-227`, `crates/fr-conformance/src/lib.rs:57-176` | Rust baseline currently routes core KV commands only; no cluster routing/redirection/failover/slot ownership/ASM interfaces are present. | Present as non-cluster baseline only. |

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-007-B01` | Normal | `CLUSTER` command with supported subcommands | Subcommand routing and arity validation are deterministic and reject unsupported forms predictably. | `FR-P2C-007-U001` | `FR-P2C-007-E001` | `cluster.command_router_contract_violation` |
| `FR-P2C-007-B02` | Normal | Multi-key command routed through slot-owner resolver | Same-slot queries execute locally when owned; foreign-slot paths classify redirection correctly. | `FR-P2C-007-U002` | `FR-P2C-007-E002` | `cluster.slot_owner_query_violation` |
| `FR-P2C-007-B03` | Normal | Redirect reply emission for foreign/unstable/down slots | `MOVED`/`ASK`/`TRYAGAIN`/`CLUSTERDOWN`/`CROSSSLOT` error text and formatting remain contract-compatible. | `FR-P2C-007-U003` | `FR-P2C-007-E003` | `cluster.redirect_reply_contract_violation` |
| `FR-P2C-007-B04` | Normal | `ASKING`/`READONLY`/`READWRITE` client mode toggles | Client cluster-mode flags alter read/redirect behavior exactly as expected across subsequent requests. | `FR-P2C-007-U004` | `FR-P2C-007-E004` | `cluster.client_mode_flag_transition_violation` |
| `FR-P2C-007-B05` | Edge | Client blocked on key while slot ownership changes | Blocked client is redirected/unblocked promptly; no indefinite waiting on no-longer-owned slot. | `FR-P2C-007-U005` | `FR-P2C-007-E005` | `cluster.blocked_client_redirect_violation` |
| `FR-P2C-007-B06` | Edge | Startup data/config drift with populated unassigned slots | Reconciliation path claims/deletes slots as required, preserving deterministic repair envelope. | `FR-P2C-007-U006` | `FR-P2C-007-E006` | `cluster.verify_config_with_data_violation` |
| `FR-P2C-007-B07` | Edge | Config-epoch collision between masters | Lexicographic tie-break and epoch bump resolve collision without split-brain ambiguity. | `FR-P2C-007-U007` | `FR-P2C-007-E007` | `cluster.config_epoch_collision_resolution_violation` |
| `FR-P2C-007-B08` | Normal | Cluster state recomputation and slot ownership updates | `CLUSTER_OK`/`CLUSTER_FAIL` transitions follow slot coverage/quorum/rejoin delay invariants. | `FR-P2C-007-U008` | `FR-P2C-007-E008` | `cluster.state_transition_contract_violation` |
| `FR-P2C-007-B09` | Adversarial | Cluster bus packet with malformed type/length/version fields | Packet parser fails closed and avoids unsafe state mutation or partial-parse acceptance. | `FR-P2C-007-U009` | `FR-P2C-007-E009` | `cluster.packet_parse_failclosed_violation` |
| `FR-P2C-007-B10` | Adversarial | Failover election with stale data or missing quorum | Failover path blocks promotion until preconditions and vote thresholds are satisfied. | `FR-P2C-007-U010` | `FR-P2C-007-E010` | `cluster.failover_election_contract_violation` |
| `FR-P2C-007-B11` | Adversarial | ASM migration sees cross-slot command during propagating phase | Migration task is deterministically canceled via before-sleep path, preventing unsafe split-stream state. | `FR-P2C-007-U011` | `FR-P2C-007-E011` | `cluster.asm_crossslot_cancel_violation` |
| `FR-P2C-007-B12` | Adversarial | Hardened mode receives non-allowlisted cluster deviation | Non-allowlisted behavior is rejected and packet remains strict-equivalent fail-closed. | `FR-P2C-007-U012` | `FR-P2C-007-E012` | `cluster.hardened_nonallowlisted_rejected` |

## High-risk traceability and structured-log contract

For all `FR-P2C-007-U*` and `FR-P2C-007-E*` rows, logs must include:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-007`)
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

- Unit/property: `fr_cluster_phase2c_packet_007`
- E2E/integration: `fr_runtime_phase2c_packet_007`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-007-CLAIM-01` |
| `evidence_id` | `FR-P2C-007-EVID-LEGACY-CLUSTER-001` |
| Hotspot evidence | `A03`, `A16`, `A29` (routing/redirect core, failover election, ASM cross-slot safety) |
| Mapped graveyard section IDs | `AG-DET-04` (deterministic state reduction), `AG-NET-06` (distributed liveness/gossip), `AG-SEC-11` (fail-closed policy) |
| Baseline comparator | Legacy Redis cluster execution path (`cluster.c`, `cluster_legacy.c`, `cluster_asm.c`, `server.c`) |
| EV score | `2.8` |
| Priority tier | `S` |
| Adoption wedge | Implement slot-owner + redirect core first, then state/failover reducers, then ASM sidecar gates |
| Budgeted mode defaults | Strict: `FailClosed`; Hardened: bounded defenses only on allowlist |
| Deterministic exhaustion behavior | Hardened budget exhaustion forces strict-equivalent fail-closed and emits `cluster.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_007`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007` |

## Expected-loss decision model

States:

- `S0`: contract-preserving cluster behavior
- `S1`: recoverable bounded condition (allowlisted)
- `S2`: unsafe routing/state/failover divergence condition

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

- `P(S1|e)` rises with bounded parser/transport anomalies without slot-owner drift.
- `P(S2|e)` rises with cross-slot misclassification, redirect drift, or failover quorum/order violations.

Calibration and fallback policy:

- Calibration metric: false-negative rate on adversarial cluster suite `< 1%`.
- Fallback trigger: calibration breach in two consecutive windows or `P(S2|e) >= 0.30`.
- Trigger behavior: disable hardened deviations for packet scope and enforce strict fail-closed mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever (downstream execution target):

- `LEV-007-01`: deterministic slot-owner decision cache keyed by `(slot, config_epoch, client_mode_flags)` with strict invalidation on slot/epoch/migration transitions.

Required loop artifacts and paths:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-007/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-007/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-007/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-007/isomorphism_report.md`

Replay commands (strict/hardened):

- `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_007_STRICT`
- `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_007_STRICT`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_HARDENED`

## Reproducibility and provenance pack references

Required pack (to be produced and linked by downstream packet beads):

- `artifacts/phase2c/FR-P2C-007/env.json`
- `artifacts/phase2c/FR-P2C-007/manifest.json`
- `artifacts/phase2c/FR-P2C-007/repro.lock`
- `artifacts/phase2c/FR-P2C-007/LEGAL.md` (mandatory if IP/provenance risk is detected)

## Confidence notes

- High confidence for routing/redirection/failover anchor extraction from `cluster.c`, `cluster_legacy.c`, and `server.c`.
- Medium confidence for ASM sidecar coverage scope because downstream packet contracts may further narrow migration-surface requirements.
- High confidence that current Rust code has no material cluster implementation surface and requires new module boundaries.
