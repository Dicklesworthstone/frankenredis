# FR-P2C-007 Rust Implementation Plan

Packet: `FR-P2C-007`  
Scope: cluster behavior (scoped) parity backbone  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible cluster behavior for FR-P2C-007 with:

- deterministic slot-owner and redirection decisions,
- explicit strict/hardened compatibility boundaries,
- fail-closed handling for cluster routing/state/failover ambiguities,
- traceable unit/e2e evidence for each `C01..C16` and `T01..T11` row.

## 2) Module boundary skeleton

### `crates/fr-command` (cluster command-surface seam)

1. `cluster_router.rs`
   - `CLUSTER` subcommand dispatch/arity validation contract (`C01`)
2. `slot_resolver.rs`
   - slot extraction and cross-slot detection (`C02`)
3. `redirect_reply.rs`
   - deterministic reply shaping for `MOVED`/`ASK`/`TRYAGAIN`/`CLUSTERDOWN` (`C04`)
4. `client_cluster_mode.rs`
   - `ASKING`/`READONLY`/`READWRITE` client mode transitions (`C07`)
5. `cluster_admin_cmd.rs`
   - scoped admin surfaces (`MEET`, slot update, failover command policy) (`C24`-derived)

### `crates/fr-runtime` (cluster state + policy host seam)

1. `cluster_state.rs`
   - slot ownership map, owner bitmap consistency, cluster `OK/FAIL` reducer (`C14`)
2. `cluster_query_gate.rs`
   - `getNodeByQuery`-equivalent pre-dispatch routing/redirect classification (`C03`)
3. `blocked_redirect.rs`
   - blocked-client redirect safety checks (`C05`)
4. `cluster_reconcile.rs`
   - startup data/config reconciliation (`C08`)
5. `cluster_epoch.rs`
   - config-epoch collision resolution and persistence hooks (`C09`)
6. `cluster_bus.rs`
   - cluster packet parser/state reducer fail-closed path (`C10`)
7. `failover_fsm.rs`
   - failover preconditions/quorum and deterministic transition policy (`C11`)
8. `asm_sidecar.rs`
   - syncslots control plane and cross-slot cancel guardrails (`C15`)
9. `cluster_observability.rs`
   - packet-scoped `cluster.*` reason-code emission

### `crates/fr-eventloop` (scheduler seam)

1. `cluster_cron.rs`
   - liveness/reconnect/timeout/failover trigger cadence (`C12`)
2. `cluster_before_sleep.rs`
   - deferred cluster todo handling order (`C13`)

### `crates/fr-config` (policy seam)

1. `cluster_policy.rs`
   - strict/hardened allowlist boundaries for cluster packet scope (`C16`)
2. `cluster_runtime_flags.rs`
   - cluster mode feature/config flags consumed by runtime/command layers

### `crates/fr-conformance` (verification seam)

1. packet fixture wiring for FR-P2C-007 strict/hardened mode runs
2. contract-row assertions (`C01..C16`)
3. threat-row adversarial assertions (`T01..T11`)

## 3) Data model invariants

1. Cluster command routing invariant (`I01`) must preserve deterministic dispatch.
2. Slot extraction invariant (`I02`) must classify cross-slot deterministically.
3. Slot-owner decision invariant (`I03`) must preserve redirect classification semantics.
4. Redirect reply invariant (`I04`) must preserve exact observable reply families.
5. Blocked-client safety invariant (`I05`) must prevent indefinite unowned-slot waiting.
6. Client mode invariant (`I06`) must preserve ASKING/READONLY/READWRITE transitions.
7. Reconciliation invariant (`I07`) must deterministically repair/reject data/config drift.
8. Epoch collision invariant (`I08`) must converge deterministically.
9. Parser fail-closed invariant (`I09`) must reject malformed cluster packets pre-mutation.
10. Scheduler/failover invariant (`I10`) must preserve cron/beforeSleep/election ordering.
11. Slot-state reducer invariant (`I11`) must preserve owner/cluster-state consistency.
12. Hardened policy invariant (`I12`) must reject non-allowlisted deviations.

## 4) Error taxonomy (packet-specific)

1. `ClusterError::CommandRouterContractViolation`
2. `ClusterError::CrossSlotDetectionViolation`
3. `ClusterError::SlotOwnerQueryViolation`
4. `ClusterError::RedirectReplyContractViolation`
5. `ClusterError::BlockedClientRedirectViolation`
6. `ClusterError::SlotsReplyShapeViolation`
7. `ClusterError::ClientModeFlagTransitionViolation`
8. `ClusterError::VerifyConfigWithDataViolation`
9. `ClusterError::ConfigEpochCollisionResolutionViolation`
10. `ClusterError::PacketParseFailClosedViolation`
11. `ClusterError::FailoverElectionContractViolation`
12. `ClusterError::CronLivenessPolicyViolation`
13. `ClusterError::BeforeSleepDeferredTaskViolation`
14. `ClusterError::SlotOwnerStateUpdateViolation`
15. `ClusterError::AsmSyncslotsStateMachineViolation`
16. `ClusterError::HardenedDeviationRejected`

Each error maps directly to packet `cluster.*` `reason_code` outputs.

## 5) Staged implementation sequence (risk-minimizing)

1. **Stage D1**: `CLUSTER` router + client mode commands (`C01`, `C07`)
2. **Stage D2**: slot extraction and pre-dispatch owner query + redirect classifier (`C02`, `C03`, `C04`)
3. **Stage D3**: blocked-client redirect bridge for wait/key operations (`C05`)
4. **Stage D4**: `CLUSTER SLOTS` response and startup reconcile pipeline (`C06`, `C08`)
5. **Stage D5**: epoch collision resolver + slot/state reducer (`C09`, `C14`)
6. **Stage D6**: cluster packet parser fail-closed path (`C10`)
7. **Stage D7**: failover FSM precondition/quorum engine (`C11`)
8. **Stage D8**: eventloop `clusterCron` + `clusterBeforeSleep` integration (`C12`, `C13`)
9. **Stage D9**: ASM syncslots sidecar guardrails and cancellation path (`C15`)
10. **Stage D10**: hardened allowlist enforcement + conformance adversarial sweep (`C16`, `T11`)

## 6) Unit/property test matrix

| Test ID | Contract rows | Threat IDs | Type | Expected result |
|---|---|---|---|---|
| `FR-P2C-007-U001` | `C01` | - | unit | cluster subcommand dispatch parity |
| `FR-P2C-007-U002` | `C02` | `T01` | adversarial unit | deterministic cross-slot classification |
| `FR-P2C-007-U003` | `C03` | `T01` | adversarial unit | slot-owner query + redirect class correctness |
| `FR-P2C-007-U004` | `C04` | `T02` | unit | redirect reply shape parity |
| `FR-P2C-007-U005` | `C05` | `T03` | adversarial unit | blocked-client redirect/unblock safety |
| `FR-P2C-007-U006` | `C06` | - | unit | `CLUSTER SLOTS` response shape contract |
| `FR-P2C-007-U007` | `C07` | `T04` | unit | ASKING/READONLY/READWRITE flag transitions |
| `FR-P2C-007-U008` | `C08` | `T05` | adversarial unit | startup data/config reconciliation behavior |
| `FR-P2C-007-U009` | `C09` | `T06` | unit | config-epoch collision convergence |
| `FR-P2C-007-U010` | `C10` | `T07` | adversarial unit | packet parser fail-closed behavior |
| `FR-P2C-007-U011` | `C11` | `T08` | adversarial unit | failover election precondition/quorum enforcement |
| `FR-P2C-007-U012` | `C15`, `C16` | `T11` | policy/adversarial unit | ASM cross-slot cancel + non-allowlisted reject |
| `FR-P2C-007-U013` | `C12`, `C13` | `T09` | integration unit | cron/beforeSleep order and trigger semantics |
| `FR-P2C-007-U014` | `C14` | `T10` | unit | slot ownership mutation + cluster state reducer parity |

## 7) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-007-E001` | `C01` | - | cluster command router parity |
| `FR-P2C-007-E002` | `C02` | `T01` | cross-slot request rejected deterministically |
| `FR-P2C-007-E003` | `C03` | `T01` | slot-owner query chooses deterministic redirect/local path |
| `FR-P2C-007-E004` | `C04` | `T02` | redirect reply family/shape parity |
| `FR-P2C-007-E005` | `C05` | `T03` | blocked-client redirect safety under slot change |
| `FR-P2C-007-E006` | `C06` | - | slot map response compatibility |
| `FR-P2C-007-E007` | `C07` | `T04` | client mode flag transition behavior |
| `FR-P2C-007-E008` | `C08` | `T05` | startup reconcile deterministic repair/reject path |
| `FR-P2C-007-E009` | `C09` | `T06` | epoch collision resolution behavior |
| `FR-P2C-007-E010` | `C10` | `T07` | malformed packet fail-closed handling |
| `FR-P2C-007-E011` | `C11` | `T08` | failover quorum/precondition enforcement |
| `FR-P2C-007-E012` | `C15`, `C16` | `T11` | ASM cross-slot safety + hardened non-allowlisted reject |
| `FR-P2C-007-E013` | `C12`, `C13` | `T09` | cron/beforeSleep deferred task ordering |
| `FR-P2C-007-E014` | `C14` | `T10` | slot/state reducer and cluster state transition parity |

## 8) Structured logging boundary interface

Cluster boundaries (`cluster_router`, `slot_resolver`, `cluster_query_gate`,
`redirect_reply`, `blocked_redirect`, `cluster_state`, `cluster_epoch`,
`cluster_bus`, `failover_fsm`, `cluster_cron`, `cluster_before_sleep`,
`asm_sidecar`) must emit:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 9) Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-007-CLAIM-04` |
| `evidence_id` | `FR-P2C-007-EVID-PLAN-001` |
| Hotspot evidence | `D2`, `D7`, `D9` (routing classifier, failover FSM, ASM guardrails) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-NET-06`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis cluster state-machine path |
| EV score | `2.9` |
| Priority tier | `S` |
| Adoption wedge | Land slot/redirect core first, then failover/scheduler reducers, then ASM and hardened policy |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` (allowlist only) |
| Deterministic exhaustion behavior | Hardened budget exhaustion => strict-equivalent fail-closed + `cluster.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_007`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_HARDENED` |

## 10) Expected-loss decision model

States:

- `S0`: implementation preserves cluster contract behavior
- `S1`: bounded hardened condition requiring allowlisted mitigation
- `S2`: unsafe routing/state/failover divergence condition

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

- `P(S1|e)` from bounded parser/metadata anomalies with no routing/epoch drift.
- `P(S2|e)` from slot-owner misclassification, failover quorum/order drift, or unsafe ASM signals.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive windows with calibration breach or critical-row drift (`C03`, `C10`, `C11`, `C15`, `C16`).
- Fallback behavior: disable hardened deviations and force strict fail-closed packet mode.

## 11) One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-007-04`: deterministic slot-owner and redirect memoization keyed by `(slot, config_epoch, client_mode_flags, cluster_state_epoch)` with strict invalidation on slot/epoch/task transitions.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-007/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-007/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-007/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-007/isomorphism_report.md`

## 12) Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-007/env.json`
- `artifacts/phase2c/FR-P2C-007/manifest.json`
- `artifacts/phase2c/FR-P2C-007/repro.lock`
- `artifacts/phase2c/FR-P2C-007/LEGAL.md` (required if IP/provenance risk is found)

## 13) Verification command set (local + CI replay)

- `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_007`
- `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_007`
- `rch exec -- cargo test -p fr-eventloop -- --nocapture FR_P2C_007`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_STRICT`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007_HARDENED`
- `rch exec -- cargo clippy --workspace --all-targets -- -D warnings`
