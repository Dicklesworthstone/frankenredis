# FR-P2C-009 Risk Note

Packet: `FR-P2C-009`  
Subsystem: TLS/config boundary  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-009/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-009/contract_table.md`

## Compatibility envelope

- `strict` mode: preserve Redis-observable TLS/config behavior for scoped packet
  surfaces (parse/build/apply/listener/handshake/rewrite).
- `hardened` mode: permit only bounded controls
  (`BoundedParserDiagnostics`, `MetadataSanitization`, `ResourceClamp`) with no
  outward API/ordering drift.
- Unknown or non-allowlisted behavior is `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-009-T01` | Protocol downgrade/parse drift | Invalid/legacy protocol tokens accepted or normalized incorrectly | `C01`, `I01` | Deterministic parse/reject semantics | Same | `FR-P2C-009-U001` | `FR-P2C-009-E001` | `tlscfg.protocols_parse_contract_violation` | High |
| `FR-P2C-009-T02` | Context construction integrity failure | Missing/invalid cert/key/CA/cipher material causes partial context apply | `C02`, `C03`, `I02`, `I03` | Atomic failure on invalid context; no partial apply | Same | `FR-P2C-009-U002` | `FR-P2C-009-E002` | `tlscfg.context_build_contract_violation`, `tlscfg.atomic_reconfigure_violation` | Critical |
| `FR-P2C-009-T03` | Runtime reconfigure torn state | `CONFIG SET` TLS mutation leaves stale/mixed contexts or listeners | `C03`, `C11`, `C12`, `I03`, `I09` | Deterministic reconfigure via connection-type hook and atomic swap | Same | `FR-P2C-009-U010` | `FR-P2C-009-E010` | `tlscfg.runtime_apply_contract_violation`, `tlscfg.connection_type_configure_violation` | Critical |
| `FR-P2C-009-T04` | Listener policy bypass | TLS listeners active while disabled, or inactive when required | `C04`, `I04` | Listener state must match policy/port flags | Same | `FR-P2C-009-U004` | `FR-P2C-009-E004` | `tls.listener_bootstrap_contract_violation` | High |
| `FR-P2C-009-T05` | Client-auth bypass | Accepted sessions skip configured verify mode | `C05`, `I05` | Verify mode strictly follows `tls-auth-clients` policy | Same | `FR-P2C-009-U005` | `FR-P2C-009-E005` | `tls.handshake_verify_policy_violation` | Critical |
| `FR-P2C-009-T06` | Handshake/I/O state machine lockup | WANT/ERROR transitions or event subscriptions drift and block progress | `C06`, `C07`, `I06` | Deterministic state transitions with bounded non-blocking semantics | Same | `FR-P2C-009-U005` | `FR-P2C-009-E005` | `tls.io_state_transition_violation`, `tls.io_budget_errno_contract_violation` | Critical |
| `FR-P2C-009-T07` | Peer identity spoof/drift | Wrong cert field extraction used for identity mapping | `C08`, `I05` | Deterministic configured-field extraction only | Same | `FR-P2C-009-U007` | `FR-P2C-009-E007` | `tls.peer_identity_contract_violation` | High |
| `FR-P2C-009-T08` | Directive policy mutation drift | TLS directive mutability/sensitivity/validation envelope bypassed | `C09`, `I07` | Directive registry and validation behavior remain deterministic | Same | `FR-P2C-009-U008` | `FR-P2C-009-E008` | `tlscfg.directive_registry_contract_violation` | High |
| `FR-P2C-009-T09` | Bind/apply atomicity failure | TCP/TLS listener transition leaves mixed state after failure | `C10`, `I08` | Rollback-safe atomic listener transitions | Same | `FR-P2C-009-U009` | `FR-P2C-009-E009` | `tlscfg.bind_atomicity_violation` | Critical |
| `FR-P2C-009-T10` | Persistence/rewrite drift | TLS directives lost or reordered incompatibly in rewrite output | `C13`, `C14`, `I10`, `I11` | Operational knobs and rewrite persistence semantics remain deterministic | Same | `FR-P2C-009-U011` | `FR-P2C-009-E011` | `tlscfg.operational_knob_contract_violation`, `tlscfg.rewrite_persistence_violation` | High |
| `FR-P2C-009-T11` | Hardened policy downgrade | Non-allowlisted TLS/config deviation proceeds in hardened mode | `C15`, `C16`, `I12` | N/A (strict fail-closed baseline) | Reject non-allowlisted deviations | `FR-P2C-009-U013` | `FR-P2C-009-E013` | `tlscfg.hardened_nonallowlisted_rejected`, `tlscfg.hardened_policy_violation` | Critical |

## Fail-closed rules

1. Invalid TLS protocol/certificate/config states must not partially mutate runtime context (`C01`-`C03`).
2. Listener/bind apply failures must not leave mixed TCP/TLS state (`C04`, `C10`).
3. Handshake/I/O state-machine inconsistencies must abort transition and preserve deterministic error state (`C05`-`C07`).
4. Config rewrite/persistence drift is fail-closed for packet promotion (`C14`).
5. Non-allowlisted hardened deviations are rejected (`C16`).

## Audit-log requirements

All threat detections/rejections/recoveries must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-009`
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
| `claim_id` | `FR-P2C-009-CLAIM-03` |
| `evidence_id` | `FR-P2C-009-EVID-RISK-001` |
| Hotspot evidence | `T02`, `T06`, `T09` (context atomicity, TLS I/O state machine, bind rollback safety) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-SEC-11`, `AG-NET-06` |
| Baseline comparator | Legacy Redis TLS/config threat surface (`tls.c` + `config.c` + listener hooks) |
| EV score | `3.2` |
| Priority tier | `S` |
| Adoption wedge | Enforce parse/build/apply atomicity and listener safety before throughput-oriented tuning |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | Budget exhaustion forces strict-equivalent fail-closed and emits `tlscfg.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-config -- --nocapture FR_P2C_009`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_009_HARDENED` |

## Expected-loss decision model

### States

- `S0`: contract-preserving TLS/config operation
- `S1`: recoverable bounded condition (allowlisted)
- `S2`: unsafe TLS/config divergence condition

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

- `P(S1|e)`: bounded parser/metadata anomalies without context/listener drift.
- `P(S2|e)`: context apply atomicity breach, handshake-state drift, or bind/rewrite contract violation.

Decision policy:

- if posterior(`S2`) > `0.30`, enforce `A2` fail-closed.
- if posterior(`S1`) > `0.40` and deviation category is allowlisted, use `A1`.
- otherwise use `A0`.

## Calibration and fallback trigger

- Calibration metric: false-negative rate on adversarial TLS/config suite `< 1%`.
- Fallback trigger: unresolved strict-mode drift on critical rows (`C03`, `C10`, `C11`, `C16`) blocks packet promotion.
- Budget exhaustion policy: hardened exhaustion across two consecutive windows reverts packet to strict fail-closed mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-009-03`: deterministic TLS apply/handshake decision cache keyed by `(tls_cfg_digest, listener_mask, io_state_bucket, policy_epoch)` with strict invalidation on config and listener transitions.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-009/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-009/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-009/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-009/isomorphism_report.md`

## Replay commands

- Config-layer threat suite: `rch exec -- cargo test -p fr-config -- --nocapture FR_P2C_009`
- Runtime threat suite: `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_009`
- E2E threat suite: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_009`
- Hardened replay: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_009_HARDENED`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-009/env.json`
- `artifacts/phase2c/FR-P2C-009/manifest.json`
- `artifacts/phase2c/FR-P2C-009/repro.lock`
- `artifacts/phase2c/FR-P2C-009/LEGAL.md` (required when IP/provenance risk is plausible)

## Residual risks

- Current Rust code has no TLS connection type/runtime handshake implementation; risk controls remain contractual until implementation beads land.
- TLS listener/config rewrite parity depends on future runtime/config persistence surfaces not yet implemented.
- Handshake state-machine parity requires event-loop integration work that is currently absent in Rust baseline.
