# FR-P2C-007 Contract Table

Packet: `FR-P2C-007`  
Subsystem: cluster behavior (scoped)  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-007/legacy_anchor_map.md`

## Contract row schema (normative)

Each row defines:

- `trigger`: deterministic cluster event.
- `preconditions`: required state before contract evaluation.
- `strict_contract`: Redis-observable behavior that must match legacy semantics.
- `hardened_contract`: bounded defensive behavior preserving API/ordering contract.
- `fail_closed_boundary`: mandatory hard-failure edge.
- `unit_trace` / `e2e_trace`: required verification mapping.
- `reason_codes`: deterministic diagnostics required on mismatch.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-007-C01` | `CLUSTER` command dispatch | Cluster mode enabled, subcommand provided | Subcommand routing and arity validation match legacy `clusterCommand` semantics. | Same; diagnostics may be richer. | Unsupported/invalid subcommand accepted or misrouted. | `FR-P2C-007-U001` | `FR-P2C-007-E001` | `cluster.command_router_contract_violation` |
| `FR-P2C-007-C02` | Multi-key slot extraction | Keys are extracted for command | Single-slot commands resolve one slot; cross-slot commands are classified as `CROSSSLOT`. | Same. | Cross-slot request treated as same-slot. | `FR-P2C-007-U002` | `FR-P2C-007-E002` | `cluster.crossslot_detection_violation` |
| `FR-P2C-007-C03` | Slot-owner query for command | Slot id known or derivable | `getNodeByQuery`-equivalent decision yields deterministic owner/redirect classification (`NONE`, `MOVED`, `ASK`, `TRYAGAIN`, `CLUSTERDOWN`). | Same with bounded diagnostics. | Wrong owner/redirect class returned for slot state. | `FR-P2C-007-U003` | `FR-P2C-007-E003` | `cluster.slot_owner_query_violation` |
| `FR-P2C-007-C04` | Redirect reply emission | Redirect code + context available | User-visible redirect error text/shape (`-MOVED`, `-ASK`, `-TRYAGAIN`, `-CLUSTERDOWN`, `-CROSSSLOT`) is contract-compatible. | Same. | Redirect code mapped to wrong reply family or malformed payload. | `FR-P2C-007-U004` | `FR-P2C-007-E004` | `cluster.redirect_reply_contract_violation` |
| `FR-P2C-007-C05` | Blocked-client slot ownership check | Client blocked on key operation | Blocked client is redirected/unblocked when slot ownership changes or cluster health invalidates wait path. | Same. | Blocked client can wait indefinitely on unowned/down slot. | `FR-P2C-007-U005` | `FR-P2C-007-E005` | `cluster.blocked_client_redirect_violation` |
| `FR-P2C-007-C06` | `CLUSTER SLOTS` response generation | Slot ownership map available | Response shape/range packing and node tuple ordering are deterministic and legacy-compatible. | Same. | Slot map response omits/reshapes mandatory fields. | `FR-P2C-007-U006` | `FR-P2C-007-E006` | `cluster.slots_reply_shape_violation` |
| `FR-P2C-007-C07` | `ASKING`/`READONLY`/`READWRITE` client mode transitions | Cluster mode enabled | Client flags transition deterministically and alter read/redirect behavior per contract. | Same. | Mode transition mutates unrelated behavior or skips required flag changes. | `FR-P2C-007-U007` | `FR-P2C-007-E007` | `cluster.client_mode_flag_transition_violation` |
| `FR-P2C-007-C08` | Startup data/config reconciliation | Node is master; cluster redirection active | `verifyClusterConfigWithData`-equivalent behavior claims unassigned populated slots and deletes unowned-slot keys deterministically. | Same. | Data/config drift accepted without deterministic repair path. | `FR-P2C-007-U008` | `FR-P2C-007-E008` | `cluster.verify_config_with_data_violation` |
| `FR-P2C-007-C09` | Config-epoch collision handling | Two masters share configEpoch | Collision is resolved deterministically by epoch bump/tie-break policy and config persistence/broadcast path. | Same. | Epoch collision persists or diverges non-deterministically. | `FR-P2C-007-U009` | `FR-P2C-007-E009` | `cluster.config_epoch_collision_resolution_violation` |
| `FR-P2C-007-C10` | Cluster bus packet ingest | Cluster link receives packet | Packet parser validates type/version/length/extensions fail-closed and mutates state only on valid frames. | Same with bounded diagnostics. | Malformed packet advances cluster/failover state. | `FR-P2C-007-U010` | `FR-P2C-007-E010` | `cluster.packet_parse_failclosed_violation` |
| `FR-P2C-007-C11` | Failover election progression | Replica role and election preconditions evaluated | Failover election requires preconditions, deterministic delay/rank policy, quorum vote threshold, and epoch update on success. | Same. | Failover proceeds without quorum/preconditions. | `FR-P2C-007-U011` | `FR-P2C-007-E011` | `cluster.failover_election_contract_violation` |
| `FR-P2C-007-C12` | `clusterCron` liveness/timeout cycle | Cluster cron tick active | Reconnect/ping/timeout/PFAIL transitions and migration/failover triggers follow deterministic cron policy. | Same; bounded clamps may emit diagnostics only. | Cron skips required safety transitions or emits unsafe retries. | `FR-P2C-007-U013` | `FR-P2C-007-E013` | `cluster.cron_liveness_policy_violation` |
| `FR-P2C-007-C13` | `clusterBeforeSleep` deferred tasks | Deferred TODO flags present | Deferred cluster actions (`manual failover`, `state update`, `save config`, `broadcast`) execute in contract order. | Same. | Deferred actions reorder/skip in behavior-changing way. | `FR-P2C-007-U013` | `FR-P2C-007-E013` | `cluster.before_sleep_deferred_task_violation` |
| `FR-P2C-007-C14` | Slot ownership mutation and state recompute | Slot assignment change or state update trigger | Slot add/delete and cluster state recompute preserve owner bitmap consistency and deterministic `OK/FAIL` transitions. | Same. | Slot/state reducer accepts inconsistent owner/quorum transitions. | `FR-P2C-007-U014` | `FR-P2C-007-E014` | `cluster.slot_owner_state_update_violation`, `cluster.state_transition_contract_violation` |
| `FR-P2C-007-C15` | ASM syncslots migration control plane event | ASM task active for import/migrate range | ASM task publication, cron/before-sleep progression, and cross-slot propagation cancellation preserve migration safety boundaries. | Same with bounded diagnostics. | Cross-slot propagation accepted during migration task. | `FR-P2C-007-U012` | `FR-P2C-007-E012` | `cluster.asm_syncslots_state_machine_violation`, `cluster.asm_crossslot_cancel_violation` |
| `FR-P2C-007-C16` | Hardened non-allowlisted deviation candidate | Mode=`hardened` and deviation unresolved | Strict-equivalent fail-closed baseline unless deviation category is explicitly allowlisted. | Only allowlisted bounded defenses may proceed with policy evidence. | Non-allowlisted deviation changes outward cluster semantics. | `FR-P2C-007-U012` | `FR-P2C-007-E012` | `cluster.hardened_nonallowlisted_rejected`, `cluster.hardened_policy_violation` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-007-I01` | Cluster command routing determinism | Required | Required |
| `FR-P2C-007-I02` | Single-slot/cross-slot extraction correctness | Required | Required |
| `FR-P2C-007-I03` | Slot-owner query + redirect classification correctness | Required | Required |
| `FR-P2C-007-I04` | Redirect reply shape compatibility | Required | Required |
| `FR-P2C-007-I05` | Blocked-client redirect safety | Required | Required |
| `FR-P2C-007-I06` | Client mode flag transition semantics | Required | Required |
| `FR-P2C-007-I07` | Startup data/config reconciliation semantics | Required | Required |
| `FR-P2C-007-I08` | Config-epoch collision convergence | Required | Required |
| `FR-P2C-007-I09` | Cluster packet parser fail-closed behavior | Required | Required |
| `FR-P2C-007-I10` | Failover/cron/before-sleep scheduling safety | Required | Required |
| `FR-P2C-007-I11` | Slot ownership + cluster state reducer consistency | Required | Required |
| `FR-P2C-007-I12` | Non-allowlisted hardened deviation handling | N/A (strict fail-closed baseline) | Reject non-allowlisted deviations |

## Allowed hardened deviations (bounded)

- `BoundedParserDiagnostics`: richer parser diagnostics for cluster-bus and command parsing without behavior drift.
- `MetadataSanitization`: bounded metadata sanitation only when strict-equivalent.
- `ResourceClamp`: bounded buffering/clamp behavior that does not alter visible routing/failover contract.

Non-allowlisted behavior differences are rejected and treated as `fail_closed`.

## Structured-log contract for FR-P2C-007 rows

Each contract-row verification result (pass/fail and divergence checks) must emit:

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

## Replay command templates

- Unit/property: `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_007`
- Runtime integration: `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_007`
- Integration/E2E: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007`
- Strict-mode sweep: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_STRICT`
- Hardened-mode sweep: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_HARDENED`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-007-CLAIM-02` |
| `evidence_id` | `FR-P2C-007-EVID-CONTRACT-001` |
| Hotspot evidence | `C03`, `C11`, `C15` (slot-owner decision, failover election, ASM migration guardrails) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-NET-06`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis cluster-state machine behavior |
| EV score | `2.9` |
| Priority tier | `S` |
| Adoption wedge | Implement routing/redirect core first, then failover/state reducers, then ASM sidecar constraints |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | On budget exhaustion force strict-equivalent fail-closed with `cluster.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_007`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_HARDENED` |

## Expected-loss decision model

States:

- `S0`: contract-preserving cluster behavior
- `S1`: bounded recoverable condition (allowlisted)
- `S2`: unsafe routing/state/failover divergence condition

Actions:

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense + evidence emission
- `A2`: fail closed and block transition

Loss matrix:

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)`: parser/transport anomalies without slot-owner drift.
- `P(S2|e)`: redirect misclassification, failover quorum/order violation, or ASM cross-slot safety breach.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive calibration breaches or critical row drift (`C03`, `C11`, `C15`, `C16`).
- Fallback behavior: disable hardened deviations and enforce strict fail-closed packet mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-007-02`: deterministic redirect decision cache keyed by `(slot, config_epoch, client_mode_flags, cluster_state_epoch)` with strict invalidation on slot/config updates.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-007/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-007/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-007/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-007/isomorphism_report.md`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-007/env.json`
- `artifacts/phase2c/FR-P2C-007/manifest.json`
- `artifacts/phase2c/FR-P2C-007/repro.lock`
- `artifacts/phase2c/FR-P2C-007/LEGAL.md` (required if IP/provenance risk is found)

## Traceability checklist

- Every contract row maps to at least one unit ID and one e2e ID.
- Every contract row declares deterministic `reason_code` values.
- Every contract row includes explicit strict/hardened expectations and fail-closed boundary.
- Cluster user-visible outcomes are explicit for replies, redirects, and state transitions.
