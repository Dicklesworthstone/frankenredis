# FR-P2C-007 Risk Note

Packet: `FR-P2C-007`  
Subsystem: cluster behavior (scoped)  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-007/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md`

## Compatibility envelope

- `strict` mode: preserve Redis-observable cluster replies, side effects, routing,
  and failover/state-order behavior for scoped cluster surfaces.
- `hardened` mode: permit only bounded controls
  (`BoundedParserDiagnostics`, `MetadataSanitization`, `ResourceClamp`) with no
  outward API/ordering drift.
- Unknown or non-allowlisted behavior is `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-007-T01` | Slot routing integrity breach | Cross-slot or wrong-owner query accepted as local operation | `C02`, `C03`, `I02`, `I03` | Reject with deterministic cross-slot/redirect classification | Same | `FR-P2C-007-U003` | `FR-P2C-007-E003` | `cluster.crossslot_detection_violation`, `cluster.slot_owner_query_violation` | Critical |
| `FR-P2C-007-T02` | Redirect contract drift | Redirect path emits wrong error family/format or endpoint metadata | `C04`, `I04` | Emit exact redirect reply class and payload contract | Same | `FR-P2C-007-U004` | `FR-P2C-007-E004` | `cluster.redirect_reply_contract_violation` | High |
| `FR-P2C-007-T03` | Blocked-client starvation | Blocked key-wait client remains blocked after slot movement/down state | `C05`, `I05` | Redirect/unblock deterministically; no indefinite wait | Same | `FR-P2C-007-U005` | `FR-P2C-007-E005` | `cluster.blocked_client_redirect_violation` | Critical |
| `FR-P2C-007-T04` | Client-mode policy bypass | `ASKING`/`READONLY`/`READWRITE` flags permit behavior outside cluster contract | `C07`, `I06` | Flag transitions and read-path exceptions remain deterministic and bounded | Same | `FR-P2C-007-U007` | `FR-P2C-007-E007` | `cluster.client_mode_flag_transition_violation` | High |
| `FR-P2C-007-T05` | Data/config divergence | Startup reconciliation fails to claim/delete inconsistent slot ownership | `C08`, `I07` | Deterministic repair or fail-closed reject on unrecoverable drift | Same | `FR-P2C-007-U008` | `FR-P2C-007-E008` | `cluster.verify_config_with_data_violation` | High |
| `FR-P2C-007-T06` | Epoch collision split-brain | Config-epoch collision persists due non-deterministic resolution | `C09`, `I08` | Resolve collision deterministically and persist/broadcast update | Same | `FR-P2C-007-U009` | `FR-P2C-007-E009` | `cluster.config_epoch_collision_resolution_violation` | Critical |
| `FR-P2C-007-T07` | Cluster-bus parser abuse | Malformed packet (type/version/length/extensions) mutates state | `C10`, `I09` | Fail closed; reject malformed packet before state mutation | Same | `FR-P2C-007-U010` | `FR-P2C-007-E010` | `cluster.packet_parse_failclosed_violation` | Critical |
| `FR-P2C-007-T08` | Failover election spoof | Replica promotion occurs without required preconditions/quorum/delay guards | `C11`, `I10` | Enforce election preconditions and quorum threshold strictly | Same | `FR-P2C-007-U011` | `FR-P2C-007-E011` | `cluster.failover_election_contract_violation` | Critical |
| `FR-P2C-007-T09` | Scheduler-order drift | `clusterCron`/`clusterBeforeSleep` ordering skips required liveness/deferred actions | `C12`, `C13`, `I10`, `I11` | Maintain deterministic hook ordering and trigger semantics | Same | `FR-P2C-007-U013` | `FR-P2C-007-E013` | `cluster.cron_liveness_policy_violation`, `cluster.before_sleep_deferred_task_violation` | High |
| `FR-P2C-007-T10` | Slot/state reducer inconsistency | Slot owner map and cluster `OK/FAIL` reducer diverge under transitions | `C14`, `I11` | Enforce owner bitmap consistency + deterministic state reducer | Same | `FR-P2C-007-U014` | `FR-P2C-007-E014` | `cluster.slot_owner_state_update_violation`, `cluster.state_transition_contract_violation` | High |
| `FR-P2C-007-T11` | Migration policy downgrade | ASM migration accepts cross-slot propagation or hardened non-allowlisted drift | `C15`, `C16`, `I12` | N/A (strict fail-closed baseline) | Reject non-allowlisted behavior and cancel unsafe migration path | `FR-P2C-007-U012` | `FR-P2C-007-E012` | `cluster.asm_crossslot_cancel_violation`, `cluster.hardened_nonallowlisted_rejected`, `cluster.hardened_policy_violation` | Critical |

## Fail-closed rules

1. Slot-owner or cross-slot ambiguity must never execute as local success (`C02`, `C03`).
2. Malformed cluster-bus packets must never mutate cluster/failover state (`C10`).
3. Failover promotion without deterministic preconditions/quorum is forbidden (`C11`).
4. Unsafe ASM cross-slot propagation must be canceled deterministically (`C15`).
5. Non-allowlisted hardened deviations are rejected (`C16`).

## Audit-log requirements

All threat detections/rejections/recoveries must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-007`
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
| `claim_id` | `FR-P2C-007-CLAIM-03` |
| `evidence_id` | `FR-P2C-007-EVID-RISK-001` |
| Hotspot evidence | `T01`, `T08`, `T11` (routing integrity, failover quorum, ASM safety/policy boundaries) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-NET-06`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis cluster threat surface (`cluster.c` + `cluster_legacy.c` + `cluster_asm.c`) |
| EV score | `3.0` |
| Priority tier | `S` |
| Adoption wedge | Enforce slot-owner/redirect and failover election guardrails before optimizing cluster paths |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | Budget exhaustion forces strict-equivalent fail-closed and emits `cluster.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_007`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_HARDENED` |

## Expected-loss decision model

### States

- `S0`: contract-preserving cluster operation
- `S1`: recoverable bounded condition (allowlisted)
- `S2`: unsafe routing/state/failover condition

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

- `P(S1|e)`: parser/transport anomalies without routing/epoch drift.
- `P(S2|e)`: owner misclassification, election/quorum violations, or unsafe ASM migration evidence.

Decision policy:

- if posterior(`S2`) > `0.30`, enforce `A2` fail-closed.
- if posterior(`S1`) > `0.40` and deviation category is allowlisted, use `A1`.
- otherwise use `A0`.

## Calibration and fallback trigger

- Calibration metric: false-negative rate on adversarial cluster suite `< 1%`.
- Fallback trigger: unresolved strict-mode drift on critical rows (`C03`, `C10`, `C11`, `C15`, `C16`) blocks packet promotion.
- Budget exhaustion policy: hardened exhaustion across two consecutive windows reverts packet to strict fail-closed mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-007-03`: deterministic blocked-client redirect eligibility cache over `(slot, owner_epoch, client_mode_flags, cluster_state_epoch)` with strict invalidation on slot/state transitions.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-007/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-007/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-007/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-007/isomorphism_report.md`

## Replay commands

- Unit threat suite: `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_007`
- Runtime threat suite: `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_007`
- E2E threat suite: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007`
- Hardened replay: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_HARDENED`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-007/env.json`
- `artifacts/phase2c/FR-P2C-007/manifest.json`
- `artifacts/phase2c/FR-P2C-007/repro.lock`
- `artifacts/phase2c/FR-P2C-007/LEGAL.md` (required when IP/provenance risk is plausible)

## Residual risks

- Current Rust cluster subsystem is absent; threat controls remain contractual until implementation beads land.
- Failover and epoch-collision paths are highly stateful and require deterministic event-order tests to prevent hidden regressions.
- ASM sidecar behavior is sensitive to command propagation ordering; cross-slot cancellation must be explicitly validated in strict and hardened modes.
