# FR-P2C-006 Rust Implementation Plan

Packet: `FR-P2C-006`  
Scope: replication state machine parity backbone  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible replication behavior for FR-P2C-006 with:

- deterministic replication state transitions,
- explicit strict/hardened compatibility boundaries,
- fail-closed handling for lineage/order ambiguities,
- traceable unit/e2e evidence for each `C01..C16` and `T01..T11` row.

## 2) Module boundary skeleton

### `crates/fr-repl` (replication-core seam)

Current state is minimal (`ReplState`/offset progress types in `lib.rs` only). This packet introduces:

1. `backlog_lifecycle.rs`
   - backlog creation/rotation, replid lineage, offset-window contracts
2. `psync_engine.rs`
   - partial-vs-full resync decision logic and reply shaping
3. `handshake_fsm.rs`
   - `PING -> AUTH? -> REPLCONF* -> PSYNC` ordering and state transitions
4. `ack_tracker.rs`
   - monotonic ACK/FACK ingestion and per-replica ack snapshots
5. `wait_gate.rs`
   - `WAIT`/`WAITAOF` threshold evaluation and unblock policy
6. `role_transition.rs`
   - `REPLICAOF` transitions, replid-shift continuity, reconnect semantics
7. `metadata_restore.rs`
   - RDB repl-metadata restore/rebase hooks
8. `threat_emit.rs`
   - packet-scoped `repl.*` reason-code emission

### `crates/fr-command` (command-surface seam)

1. `repl_commands.rs`
   - dispatch handlers for `REPLCONF`, `REPLICAOF`, `ROLE`
2. `wait_commands.rs`
   - dispatch handlers for `WAIT`, `WAITAOF`
3. command router wiring
   - route replication commands into `fr-repl` service APIs
4. `repl_errors.rs`
   - user-visible error taxonomy + packet reason-code mapping

### `crates/fr-runtime` (policy host seam)

1. `replication_host.rs`
   - runtime ownership of replication service and strict/hardened policy gate
2. `apply_delta_bridge.rs`
   - post-apply command delta forwarding to replication backlog
3. `wait_unblock_bridge.rs`
   - invoke replication wake checks after write/apply/fsync progress
4. `replication_observability.rs`
   - packet evidence emission and drift classification records

### `crates/fr-eventloop` (scheduler seam)

1. `before_sleep_repl.rs`
   - GETACK batching and WAIT/WAITAOF wake checks in pre-sleep phase
2. `replication_cron.rs`
   - timeout/keepalive/backlog-TTL periodic maintenance hooks

### `crates/fr-persist` (durability seam)

1. `repl_metadata_io.rs`
   - read/write helpers for replication metadata across snapshot/recovery
2. `waitaof_fsync_bridge.rs`
   - local fsync progress publication for WAITAOF contract checks

### `crates/fr-conformance` (verification seam)

1. packet fixture wiring for FR-P2C-006 strict/hardened mode runs
2. contract-row assertions (`C01..C16`)
3. threat-row adversarial assertions (`T01..T11`)

## 3) Data model invariants

1. Backlog/replid lifecycle invariant (`I01`) must preserve lineage safety.
2. PSYNC branch invariant (`I02`) must split partial/full paths deterministically.
3. Handshake ordering invariant (`I03`) forbids transition reordering.
4. ACK monotonicity invariant (`I04`) forbids stale/decreasing ack transitions.
5. Applied-delta proxy invariant (`I05`) permits only applied-byte propagation.
6. Role-transition invariant (`I06`) preserves replid-history continuity.
7. WAIT invariant (`I07`) requires target ack thresholds before success.
8. WAITAOF invariant (`I08`) requires local+replica durability thresholds.
9. Cron-policy invariant (`I09`) preserves timeout/backlog TTL semantics.
10. Metadata restore invariant (`I10`) preserves role-correct lineage restore.
11. Event-loop wake invariant (`I11`) guarantees deterministic unblocking checks.
12. Hardened-policy invariant (`I12`) rejects non-allowlisted behavior deviation.

## 4) Error taxonomy (packet-specific)

1. `ReplError::BacklogLifecycleContractViolation`
2. `ReplError::PsyncPartialAcceptMismatch`
3. `ReplError::PsyncFullResyncFallbackMismatch`
4. `ReplError::FullResyncReplyParseViolation`
5. `ReplError::HandshakeStateMachineMismatch`
6. `ReplError::ReplconfAckOffsetContractViolation`
7. `ReplError::AppliedDeltaProxyViolation`
8. `ReplError::RoleTransitionReplidShiftMismatch`
9. `ReplError::ReplicaofModeConstraintViolation`
10. `ReplError::WaitAckCountMismatch`
11. `ReplError::WaitaofAckSemanticsMismatch`
12. `ReplError::ReplicationCronTimeoutPolicyViolation`
13. `ReplError::RdbReplMetadataRestoreViolation`
14. `ReplError::HardenedDeviationRejected`

Each error maps directly to packet `repl.*` `reason_code` outputs.

## 5) Staged implementation sequence (risk-minimizing)

1. **Stage D1**: backlog lifecycle + replid lineage primitives (`C01`, `C14`, `T02`, `T10`)
2. **Stage D2**: PSYNC branch engine + reply parser contracts (`C02`, `C03`, `C04`, `T01`)
3. **Stage D3**: handshake FSM + REPLCONF ACK/FACK monotonic path (`C05`, `C06`, `T03`, `T04`)
4. **Stage D4**: command surface for `REPLICAOF`/`ROLE`/`WAIT`/`WAITAOF` (`C09`, `C10`, `C11`, `C12`)
5. **Stage D5**: event-loop `beforeSleep` wake bridge for WAIT/WAITAOF (`C13`, `T06`)
6. **Stage D6**: applied-stream delta proxy and backlog append invariants (`C07`)
7. **Stage D7**: role transition and reconnect safety envelope (`C08`, `T07`, `T08`)
8. **Stage D8**: RDB replication metadata restore/rebase wiring (`C15`, `T09`)
9. **Stage D9**: hardened allowlist guardrails + policy rejection (`C16`, `T11`)
10. **Stage D10**: packet conformance sweep + drift-classification closure (`C01..C16`, `T01..T11`)

## 6) Unit/property test matrix

| Test ID | Contract rows | Threat IDs | Type | Expected result |
|---|---|---|---|---|
| `FR-P2C-006-U001` | `C01`, `C02` | `T02` | unit | backlog lifecycle + partial-resync acceptance parity |
| `FR-P2C-006-U002` | `C03` | `T01` | adversarial unit | full-resync fallback on lineage mismatch |
| `FR-P2C-006-U003` | `C05` | `T03` | adversarial unit | handshake state-order enforcement |
| `FR-P2C-006-U004` | `C06` | `T04` | unit | ACK/FACK monotonic contract |
| `FR-P2C-006-U005` | `C11` | `T05` | adversarial unit | WAIT threshold gating correctness |
| `FR-P2C-006-U006` | `C12`, `C13` | `T06` | adversarial unit | WAITAOF threshold + wake consistency |
| `FR-P2C-006-U007` | `C08` | `T07` | unit | role-transition replid continuity |
| `FR-P2C-006-U008` | `C09` | `T08` | unit | REPLICAOF mode/no-op policy correctness |
| `FR-P2C-006-U009` | `C15` | `T09` | adversarial unit | metadata restore/rebase lineage safety |
| `FR-P2C-006-U010` | `C04` | `T01` | adversarial unit | fullresync/continue reply-parse contract |
| `FR-P2C-006-U011` | `C14` | `T10` | unit | cron timeout + backlog TTL policy |
| `FR-P2C-006-U012` | `C16` | `T11` | policy unit | reject non-allowlisted hardened deviations |
| `FR-P2C-006-U013` | `C07` | `T04` | unit | applied-delta proxy ordering/coverage |
| `FR-P2C-006-U014` | `C10` | `T07` | unit | ROLE reply shape + metadata parity |

## 7) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-006-E001` | `C01`, `C02` | `T02` | backlog bootstrap and partial PSYNC parity |
| `FR-P2C-006-E002` | `C03` | `T01` | lineage mismatch forces full-resync path |
| `FR-P2C-006-E003` | `C05` | `T03` | handshake ordering violations rejected |
| `FR-P2C-006-E004` | `C06` | `T04` | REPLCONF ACK/FACK monotonic behavior |
| `FR-P2C-006-E005` | `C11` | `T05` | WAIT blocks/unblocks only on thresholds |
| `FR-P2C-006-E006` | `C12`, `C13` | `T06` | WAITAOF local+replica thresholds enforced |
| `FR-P2C-006-E007` | `C08` | `T07` | REPLICAOF role-shift continuity behavior |
| `FR-P2C-006-E008` | `C09` | `T08` | REPLICAOF mode constraints and no-op policy |
| `FR-P2C-006-E009` | `C15` | `T09` | metadata restore/rebase determinism |
| `FR-P2C-006-E010` | `C04` | `T01` | PSYNC reply parser branch correctness |
| `FR-P2C-006-E011` | `C14` | `T10` | replicationCron timeout/backlog policy |
| `FR-P2C-006-E012` | `C16` | `T11` | hardened non-allowlisted reject behavior |
| `FR-P2C-006-E013` | `C07` | `T04` | applied-stream delta proxy parity |
| `FR-P2C-006-E014` | `C10` | `T07` | ROLE reply parity across role transitions |

## 8) Structured logging boundary interface

Replication boundaries (`backlog_lifecycle`, `psync_engine`, `handshake_fsm`,
`ack_tracker`, `wait_gate`, `role_transition`, `metadata_restore`,
`replication_cron`, `before_sleep_repl`, `repl_commands`) must emit:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 9) Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-006-CLAIM-04` |
| `evidence_id` | `FR-P2C-006-EVID-PLAN-001` |
| Hotspot evidence | `D2`, `D4`, `D7` (PSYNC branch, WAIT/WAITAOF, role transitions) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-NET-06`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis replication state machine (`replication.c`) |
| EV score | `2.9` |
| Priority tier | `S` |
| Adoption wedge | Implement lineage+PSYNC core first, then WAIT/WAITAOF and role transitions, then hardened policy |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` (allowlist only) |
| Deterministic exhaustion behavior | Hardened budget exhaustion => strict-equivalent fail-closed + `repl.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-repl -- --nocapture FR_P2C_006`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_006_HARDENED` |

## 10) Expected-loss decision model

States:

- `S0`: implementation preserves replication contract behavior
- `S1`: bounded hardened condition requiring allowlisted mitigation
- `S2`: unsafe history/order divergence condition

Actions:

- `A0`: continue implementation path
- `A1`: apply allowlisted bounded defense + evidence emission
- `A2`: fail-closed block and rollback stage progression

Loss matrix:

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)` from bounded parser/metadata anomalies with no lineage drift.
- `P(S2|e)` from replid-offset divergence, threshold-bypass evidence, or role drift.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive windows with calibration breach or critical-row drift (`C02`, `C05`, `C11`, `C16`).
- Fallback behavior: disable hardened deviations and force strict fail-closed packet mode.

## 11) One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-006-04`: deterministic waiter-threshold index cache keyed by `(target_offset, ack_epoch, fsync_epoch)` with strict invalidation on ack/fsync/replid transitions.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-006/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-006/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-006/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-006/isomorphism_report.md`

## 12) Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-006/env.json`
- `artifacts/phase2c/FR-P2C-006/manifest.json`
- `artifacts/phase2c/FR-P2C-006/repro.lock`
- `artifacts/phase2c/FR-P2C-006/LEGAL.md` (required if IP/provenance risk is found)

## 13) Verification command set (local + CI replay)

- `rch exec -- cargo test -p fr-repl -- --nocapture FR_P2C_006`
- `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_006`
- `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_006`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_006`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_006_STRICT`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_006_HARDENED`
- `rch exec -- cargo clippy --workspace --all-targets -- -D warnings`
