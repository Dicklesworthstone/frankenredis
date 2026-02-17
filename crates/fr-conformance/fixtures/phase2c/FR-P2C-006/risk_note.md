# FR-P2C-006 Risk Note

Packet: `FR-P2C-006`  
Subsystem: replication state machine  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-006/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-006/contract_table.md`

## Compatibility envelope

- `strict` mode: preserve Redis-observable replication replies, side effects,
  and ordering for PSYNC/SYNC/REPLCONF/WAIT/WAITAOF/ROLE surfaces.
- `hardened` mode: permit only bounded controls
  (`BoundedParserDiagnostics`, `MetadataSanitization`, `ResourceClamp`) with no
  outward API/ordering drift.
- Unknown or non-allowlisted behavior is `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-006-T01` | History divergence attack | PSYNC replid mismatch or invalid offset accepted as partial continuation | `C02`, `C03`, `I02` | Reject partial path and force full-resync branch | Same | `FR-P2C-006-U009` | `FR-P2C-006-E009` | `repl.psync_replid_or_offset_reject_mismatch` | Critical |
| `FR-P2C-006-T02` | Backlog poisoning | Backlog lifecycle/replid reset not enforced after backlog recreation | `C01`, `C14`, `I01` | Replid history reset and backlog bounds enforced | Same | `FR-P2C-006-U001` | `FR-P2C-006-E001` | `repl.backlog_lifecycle_contract_violation` | High |
| `FR-P2C-006-T03` | Handshake downgrade/confusion | Replica handshake state machine skips/reorders AUTH/REPLCONF/PSYNC stages | `C05`, `I03` | Ordered transitions only; reject invalid progression | Same | `FR-P2C-006-U003` | `FR-P2C-006-E003` | `repl.handshake_state_machine_mismatch` | Critical |
| `FR-P2C-006-T04` | ACK spoof/drift | REPLCONF ACK/FACK path updates non-monotonic/stale offsets | `C06`, `I04` | Monotonic ack/fack semantics only | Same | `FR-P2C-006-U004` | `FR-P2C-006-E004` | `repl.replconf_ack_offset_contract_violation` | High |
| `FR-P2C-006-T05` | WAIT bypass | WAIT unblocks before sufficient replica acknowledgments | `C11`, `I07` | Enforce offset-threshold semantics, block until satisfied/timeout | Same | `FR-P2C-006-U005` | `FR-P2C-006-E005` | `repl.wait_ack_count_mismatch` | Critical |
| `FR-P2C-006-T06` | WAITAOF spoof/drift | WAITAOF returns success before local fsync or replica AOF threshold | `C12`, `C13`, `I08`, `I11` | Enforce local+replica thresholds and beforeSleep wake consistency | Same | `FR-P2C-006-U006` | `FR-P2C-006-E006` | `repl.waitaof_ack_semantics_mismatch`, `repl.before_sleep_ack_wake_violation` | Critical |
| `FR-P2C-006-T07` | Role-transition inconsistency | REPLICAOF role switch fails replid-history continuity | `C08`, `C09`, `I06` | Shift/clear replid semantics preserved, dependent replicas handled safely | Same | `FR-P2C-006-U007` | `FR-P2C-006-E007` | `repl.role_transition_replid_shift_mismatch` | High |
| `FR-P2C-006-T08` | Disconnect/reconnect unsafe path | Master disconnection reconnect policy allows unsafe continuation semantics | `C08`, `C14`, `I06`, `I09` | Deterministic reconnect policy with safe fallback semantics | Same | `FR-P2C-006-U008` | `FR-P2C-006-E008` | `repl.master_disconnect_reconnect_policy_violation` | High |
| `FR-P2C-006-T09` | Metadata tampering | RDB repl metadata restore/rebase drift creates invalid lineage | `C15`, `I10` | Restore/rebase semantics must remain role-correct and deterministic | Same | `FR-P2C-006-U009` | `FR-P2C-006-E009` | `repl.rdb_repl_metadata_restore_violation` | Critical |
| `FR-P2C-006-T10` | Cron-policy bypass | Timeout/keepalive/backlog-TTL rotation skipped or altered | `C14`, `I09` | Enforce cron timeout and backlog lifecycle policy | Same | `FR-P2C-006-U011` | `FR-P2C-006-E011` | `repl.replication_cron_timeout_policy_violation`, `repl.backlog_ttl_replid_reset_violation` | High |
| `FR-P2C-006-T11` | Policy downgrade abuse | Hardened mode applies non-allowlisted replication behavior | `C16`, `I12` | N/A (strict fail-closed baseline) | Reject non-allowlisted deviation | `FR-P2C-006-U012` | `FR-P2C-006-E012` | `repl.hardened_nonallowlisted_rejected`, `repl.hardened_policy_violation` | Critical |

## Fail-closed rules

1. PSYNC history incompatibility must never continue as partial sync (`C03`).
2. Handshake state-order violations are fatal for packet correctness (`C05`).
3. WAIT/WAITAOF threshold bypass is forbidden (`C11`, `C12`, `C13`).
4. Repl metadata restore drift is fail-closed (`C15`).
5. Non-allowlisted hardened deviations are rejected (`C16`).

## Audit-log requirements

All threat detections/rejections/recoveries must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-006`
- `mode`
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-006-CLAIM-03` |
| `evidence_id` | `FR-P2C-006-EVID-RISK-001` |
| Hotspot evidence | `T01`, `T05`, `T06` (history divergence + wait semantics) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-NET-06`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis replication threat surface (`replication.c` + `server.c` + `networking.c`) |
| EV score | `3.0` |
| Priority tier | `S` |
| Adoption wedge | Enforce PSYNC branch correctness and WAIT/WAITAOF thresholds before optimizing replication paths |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | Budget exhaustion forces strict-equivalent fail-closed and emits `repl.hardened_budget_exhausted_failclosed` |
| Replay commands | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u001_psync_accepts_partial_resync_inside_window`; `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_waitaof_metamorphic_joint_threshold_semantics_hold` |

## Expected-loss decision model

### States

- `S0`: contract-preserving replication operation
- `S1`: recoverable bounded condition (allowlisted)
- `S2`: unsafe replication-order/history condition

### Actions

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense with evidence emission
- `A2`: fail closed and abort transition

### Loss matrix (lower is better)

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)`: parser/handshake anomalies without lineage drift.
- `P(S2|e)`: replid/offset divergence, WAIT/WAITAOF threshold mismatch, or metadata-restore drift.

Decision policy:

- if posterior(`S2`) > `0.30`, enforce `A2` fail-closed.
- if posterior(`S1`) > `0.40` and deviation category is allowlisted, use `A1`.
- otherwise use `A0`.

## Calibration and fallback trigger

- Calibration metric: false-negative rate on adversarial replication suite `< 1%`.
- Fallback trigger: unresolved strict-mode drift on critical rows (`C02`, `C03`, `C11`, `C12`, `C16`) blocks packet promotion.
- Budget exhaustion policy: hardened exhaustion across two consecutive windows reverts packet to strict fail-closed mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-006-03`: deterministic wait-threshold unblocking cache over `(reploffset, replica_ack_snapshot, fsynced_reploff)` with strict invalidation on ack/fsync updates.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-006/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-006/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-006/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-006/isomorphism_report.md`

## Replay commands

- Unit threat suite: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u006_waitaof_requires_local_and_replica_thresholds`
- Conformance threat replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_psync_adversarial_matrix_prefers_safe_fallbacks`
- Hardened replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_waitaof_metamorphic_joint_threshold_semantics_hold`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-006/env.json`
- `artifacts/phase2c/FR-P2C-006/manifest.json`
- `artifacts/phase2c/FR-P2C-006/repro.lock`
- `artifacts/phase2c/FR-P2C-006/LEGAL.md` (required when IP/provenance risk is plausible)

## Residual risks

- Current Rust replication subsystem is minimal; threat controls remain contractual until implementation beads land.
- WAIT/WAITAOF correctness is sensitive to ordering across event-loop/ack/fsync updates and needs dedicated adversarial coverage.
- Role-transition/replid drift risk remains high until full conformance loops exist for failover and reconnection permutations.
