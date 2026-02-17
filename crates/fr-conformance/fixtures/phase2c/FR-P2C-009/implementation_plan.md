# FR-P2C-009 Rust Implementation Plan

Packet: `FR-P2C-009`  
Scope: TLS/config boundary parity backbone  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible TLS/config behavior for FR-P2C-009 with:

- deterministic TLS config parse/build/apply semantics,
- explicit strict/hardened compatibility boundaries,
- fail-closed handling for TLS/config safety-gate ambiguity,
- traceable unit/e2e evidence for each `C01..C16` and `T01..T11` row.

## 2) Baseline architecture (as-is evidence)

Current implementation baseline (must be preserved unless contract rows require extension):

1. `crates/fr-config/src/lib.rs`
   - Owns global mode/threat/deviation policy (`Mode`, `RuntimePolicy`, `HardenedDeviationCategory`, `ThreatClass`).
   - No TLS-specific config schema, parser, validator, or rewrite/apply hooks.
2. `crates/fr-runtime/src/lib.rs`
   - Owns RESP execution, compatibility gate, and `EvidenceLedger`.
   - No TLS connection type, no listener reconfigure path, no handshake state machine.
3. `crates/fr-eventloop/src/lib.rs`
   - Provides generic tick-budget accounting (`run_tick`) with no TLS-specific scheduling hooks.
4. `crates/fr-conformance`
   - Hosts packet fixtures and evidence artifacts; FR-P2C-009 contract/risk documents are now available.

Gap summary: policy primitives exist, but TLS/config runtime surface (schema, listener lifecycle, handshake flow, runtime apply/rewrite semantics) is absent.

## 3) Module boundary skeleton (target)

### `crates/fr-config` (TLS schema + policy seam)

1. `tls_schema` boundary
   - typed TLS config struct mirroring required packet fields (`C01`, `C09`, `C13`)
2. `tls_parse` boundary
   - protocol/token and directive parsing/normalization (`C01`, `C09`)
3. `tls_validate` boundary
   - cert/key/CA/cipher invariants and safety-gate checks (`C02`, `C15`)
4. `tls_apply_plan` boundary
   - deterministic runtime-apply plan generation for context/listener updates (`C03`, `C11`, `C12`)
5. `tls_rewrite_contract` boundary
   - deterministic rewrite/persistence ordering contract (`C14`)

### `crates/fr-runtime` (TLS orchestration seam)

1. `tls_context_manager`
   - atomic context construction/swap and rollback behavior (`C02`, `C03`)
2. `tls_listener_manager`
   - listener bootstrap, bind coupling, and rollback-safe apply (`C04`, `C10`, `C11`)
3. `tls_connection_type`
   - startup/runtime configure hook surface (`C12`)
4. `tls_handshake_fsm`
   - accepted-session verify policy and WANT-state transitions (`C05`, `C06`, `C07`)
5. `tls_peer_identity`
   - deterministic configured cert-field extraction (`C08`)
6. `tls_operational_knobs`
   - `cluster-announce-tls-port` and per-cycle accept throttling semantics (`C13`)
7. `tls_evidence_bridge`
   - packet reason-code and replay metadata emission

### `crates/fr-eventloop` (scheduler seam)

1. `tls_event_interest_bridge`
   - readable/writable event interest updates from TLS FSM wants (`C06`)
2. `tls_accept_budget_hook`
   - per-cycle accept limiter integration (`C13`)

### `crates/fr-conformance` (verification seam)

1. packet fixture extensions for TLS/config parse/build/apply/listener/rewrite workflows
2. row-level assertions for `C01..C16` + adversarial `T01..T11`
3. strict/hardened drift classification and replay artifacts

## 4) Data model invariants

1. TLS protocol parse determinism invariant (`I01`) preserves supported/unsupported token behavior.
2. TLS context construction atomicity invariant (`I02`) forbids partial context visibility.
3. Runtime TLS reconfigure atomicity invariant (`I03`) preserves swap/rollback correctness.
4. Listener-policy alignment invariant (`I04`) preserves TLS listener enable/disable behavior.
5. Handshake verify-policy invariant (`I05`) preserves auth mode mapping + peer identity contract.
6. TLS I/O state-machine invariant (`I06`) preserves WANT/error/close transitions.
7. TLS directive registry invariant (`I07`) preserves mutability/sensitivity/validation policy.
8. Bind/apply rollback invariant (`I08`) preserves paired TCP/TLS transition safety.
9. Runtime apply + configure-hook invariant (`I09`) preserves deterministic config mutation flow.
10. TLS operational knob invariant (`I10`) preserves announce/accept-rate controls.
11. Rewrite persistence invariant (`I11`) preserves deterministic TLS config persistence.
12. Hardened allowlist invariant (`I12`) rejects non-allowlisted behavior changes.

## 5) Error taxonomy (packet-specific)

1. `TlsCfgError::ProtocolsParseContractViolation`
2. `TlsCfgError::ContextBuildContractViolation`
3. `TlsCfgError::AtomicReconfigureViolation`
4. `TlsCfgError::ListenerBootstrapContractViolation`
5. `TlsCfgError::HandshakeVerifyPolicyViolation`
6. `TlsCfgError::TlsIoStateTransitionViolation`
7. `TlsCfgError::TlsIoBudgetErrnoContractViolation`
8. `TlsCfgError::PeerIdentityContractViolation`
9. `TlsCfgError::DirectiveRegistryContractViolation`
10. `TlsCfgError::BindAtomicityViolation`
11. `TlsCfgError::RuntimeApplyContractViolation`
12. `TlsCfgError::ConnectionTypeConfigureViolation`
13. `TlsCfgError::OperationalKnobContractViolation`
14. `TlsCfgError::RewritePersistenceViolation`
15. `TlsCfgError::SafetyGateContractViolation`
16. `TlsCfgError::HardenedDeviationRejected`

Each error maps directly to `tlscfg.*` / `tls.*` reason codes in FR-P2C-009.

## 6) Staged implementation sequence (risk-minimizing)

1. **Stage D1**: introduce TLS config schema + directive parser + protocol normalization (`C01`, `C09`).
2. **Stage D2**: implement cert/key/CA/cipher validation and context-build gate (`C02`, `C15`).
3. **Stage D3**: implement atomic TLS context swap plan + rollback model (`C03`, `C12`).
4. **Stage D4**: implement listener bootstrap + bind-rollback-safe transitions (`C04`, `C10`).
5. **Stage D5**: implement accepted-session verify policy mapping (`C05`).
6. **Stage D6**: implement handshake/I/O FSM + event-interest bridging (`C06`, `C07`).
7. **Stage D7**: implement peer identity extraction contract (`C08`).
8. **Stage D8**: implement runtime `CONFIG SET` TLS apply flow with configure hook (`C11`).
9. **Stage D9**: implement operational knobs + rewrite persistence contract (`C13`, `C14`).
10. **Stage D10**: enforce hardened allowlist + full adversarial strict/hardened sweep (`C16`, `T11`).

## 7) Unit/property test matrix

| Test ID | Contract rows | Threat IDs | Type | Expected result |
|---|---|---|---|---|
| `FR-P2C-009-U001` | `C01` | `T01` | unit | protocol token parse/normalization contract |
| `FR-P2C-009-U002` | `C02` | `T02` | adversarial unit | context build is atomic and fails safely |
| `FR-P2C-009-U003` | `C03` | `T02` | adversarial unit | runtime context swap/rollback determinism |
| `FR-P2C-009-U004` | `C04` | `T04` | unit | listener enable/disable policy alignment |
| `FR-P2C-009-U005` | `C05`, `C06` | `T05`, `T06` | adversarial unit | verify-policy + handshake state-machine correctness |
| `FR-P2C-009-U006` | `C07` | `T06` | unit | I/O budget/errno semantics parity |
| `FR-P2C-009-U007` | `C08` | `T07` | unit | peer identity extraction contract |
| `FR-P2C-009-U008` | `C09` | `T08` | unit | directive registry mutability/sensitivity/validation invariants |
| `FR-P2C-009-U009` | `C10` | `T09` | adversarial unit | bind/apply rollback safety |
| `FR-P2C-009-U010` | `C11`, `C12` | `T03` | integration unit | runtime apply + configure-hook path determinism |
| `FR-P2C-009-U011` | `C13` | `T10` | unit | operational knob enforcement correctness |
| `FR-P2C-009-U012` | `C14` | `T10` | unit | rewrite persistence contract parity |
| `FR-P2C-009-U013` | `C15`, `C16` | `T11` | policy/adversarial unit | safety-gate fail-closed + hardened rejection |
| `FR-P2C-009-U014` | `C04`, `C06`, `C10` | `T04`, `T06`, `T09` | integration unit | listener/FSM/apply ordering invariants |

## 8) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-009-E001` | `C01` | `T01` | protocol parse/reject behavior parity |
| `FR-P2C-009-E002` | `C02` | `T02` | invalid context material fails atomically |
| `FR-P2C-009-E003` | `C03` | `T02` | runtime context swap is all-or-nothing |
| `FR-P2C-009-E004` | `C04` | `T04` | listener enablement follows policy/port state |
| `FR-P2C-009-E005` | `C05`, `C06` | `T05`, `T06` | accepted-session verify + handshake progression behavior |
| `FR-P2C-009-E006` | `C07` | `T06` | TLS I/O budget and errno semantics |
| `FR-P2C-009-E007` | `C08` | `T07` | peer identity field extraction parity |
| `FR-P2C-009-E008` | `C09` | `T08` | directive mutation and validation envelope |
| `FR-P2C-009-E009` | `C10` | `T09` | bind rollback preserves paired listener consistency |
| `FR-P2C-009-E010` | `C11`, `C12` | `T03` | runtime TLS reconfigure and configure-hook behavior |
| `FR-P2C-009-E011` | `C13` | `T10` | announce/accept-throttle operational behavior |
| `FR-P2C-009-E012` | `C14` | `T10` | rewrite persistence and restart parity |
| `FR-P2C-009-E013` | `C15`, `C16` | `T11` | safety-gate fail-closed + hardened non-allowlisted rejection |
| `FR-P2C-009-E014` | `C04`, `C06`, `C10` | `T04`, `T06`, `T09` | listener/FSM/apply ordering and rollback parity |

## 9) Structured logging boundary interface

TLS/config boundaries (schema parse/validate/apply, context manager, listener
manager, handshake FSM, rewrite contract, runtime gate) must emit:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 10) Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-009-CLAIM-04` |
| `evidence_id` | `FR-P2C-009-EVID-PLAN-001` |
| Hotspot evidence | `D3`, `D6`, `D9` (atomic reconfigure, handshake/I/O FSM, rewrite/operational knobs) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-SEC-11`, `AG-NET-06` |
| Baseline comparator | Legacy Redis TLS/config runtime state machine |
| EV score | `3.2` |
| Priority tier | `S` |
| Adoption wedge | Land schema/validate/apply atomicity first, then runtime handshake/listener orchestration, then persistence and hardened policy |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` (allowlist only) |
| Deterministic exhaustion behavior | Hardened budget exhaustion => strict-equivalent fail-closed with `tlscfg.hardened_budget_exhausted_failclosed` |
| Replay commands | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-config -- --nocapture fr_p2c_009_u001_protocol_parse_rejects_unknown_token`; `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract` |

## 11) Expected-loss decision model

States:

- `S0`: implementation preserves TLS/config contracts
- `S1`: bounded recoverable condition (allowlisted)
- `S2`: unsafe TLS/config divergence

Actions:

- `A0`: continue normal implementation path
- `A1`: apply allowlisted bounded defense + evidence emission
- `A2`: fail closed and block stage promotion

Loss matrix:

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)` from bounded parser/metadata anomalies without listener/context drift.
- `P(S2|e)` from context apply atomicity failure, handshake-state drift, or rewrite/bind contract violations.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive calibration breaches or critical-row drift (`C03`, `C10`, `C11`, `C16`).
- Fallback behavior: disable hardened deviations and enforce strict fail-closed packet mode.

## 12) One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-009-04`: deterministic TLS apply planner and handshake-transition memo keyed by `(tls_cfg_digest, listener_mask, io_state_bucket, policy_epoch)` with strict invalidation on config mutation and listener state transitions.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-009/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-009/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-009/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-009/isomorphism_report.md`

## 13) Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-009/env.json`
- `artifacts/phase2c/FR-P2C-009/manifest.json`
- `artifacts/phase2c/FR-P2C-009/repro.lock`
- `artifacts/phase2c/FR-P2C-009/LEGAL.md` (required if IP/provenance risk is plausible)

## 14) Verification command set (local + CI replay)

- `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-config -- --nocapture fr_p2c_009_u`
- `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u`
- `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-eventloop -- --nocapture fr_p2c_009_u011_`
- `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_f_differential_mode_split_contract_is_stable`
- `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_f_metamorphic_non_allowlisted_rejection_is_deterministic`
- `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_f_adversarial_tls_reason_codes_are_stable`
- `rch exec -- cargo clippy --workspace --all-targets -- -D warnings`
