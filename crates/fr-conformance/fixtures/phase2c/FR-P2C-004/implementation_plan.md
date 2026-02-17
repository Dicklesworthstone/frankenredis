# FR-P2C-004 Rust Implementation Plan

Packet: `FR-P2C-004`  
Scope: ACL and auth policy parity backbone  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible ACL/auth behavior for FR-P2C-004 with:

- deterministic authentication state transitions,
- explicit strict/hardened compatibility boundaries,
- fail-closed behavior for auth/ACL/config ambiguity paths,
- traceable unit/e2e evidence for each `C01..C15` and `T01..T11` row.

## 2) Module boundary skeleton

### `crates/fr-runtime` (admission-control seam)

Proposed decomposition:

1. `auth_state.rs`
   - per-client auth state machine (`authenticated`, bound user identity)
2. `admission_gate.rs`
   - `NOAUTH` pre-dispatch gating and command exception handling
3. `acl_enforce.rs`
   - runtime invocation of ACL evaluator and deny-path shaping
4. `threat_emit.rs`
   - packet-scoped evidence/threat event emission for auth/ACL decisions

### `crates/fr-command` (command surface seam)

1. `auth_cmd.rs`
   - `AUTH` command forms and deterministic reply/error policy
2. `hello_cmd.rs`
   - `HELLO ... AUTH` integration flow with early-fail contract
3. `acl_cmd.rs`
   - scoped ACL admin surface (`SETUSER`, `GETUSER`, `LOG`, `LOAD`, `SAVE`) plan
4. `acl_errors.rs`
   - mapping to packet reason codes + user-visible error families

### `crates/fr-config` (configuration policy seam)

1. `auth_policy.rs`
   - `requirepass` compatibility bridge to default-user policy
2. `acl_source_policy.rs`
   - startup source exclusivity checks (`aclfile` vs configured users)
3. hardened allowlist policy checks for packet-relevant categories:
   - `BoundedParserDiagnostics`
   - `MetadataSanitization`
   - `ResourceClamp`

### `crates/fr-conformance` (verification seam)

1. packet fixture wiring for FR-P2C-004 strict/hardened mode runs
2. contract-row assertions (`C01..C15`)
3. threat-row adversarial assertions (`T01..T11`)

## 3) Data model invariants

1. Bootstrap auth-state invariant (`I01`) must match default-user flags.
2. `AUTH` state transition invariant (`I02`) must be deterministic.
3. HELLO/AUTH ordering invariant (`I03`) must early-fail on auth error.
4. Noauth admission invariant (`I04`) must run before command execution.
5. ACL grammar invariant (`I05`) forbids malformed selector/user acceptance.
6. ACL reduction invariant (`I06`) must keep deterministic deny index.
7. ACL load invariant (`I07`) requires transactional rollback semantics.
8. Startup source invariant (`I08`) requires fail-closed on ambiguity.
9. `requirepass` bridge invariant (`I09`) must mirror default-user mutation semantics.
10. ACL log invariant (`I10`) must preserve bounded grouping behavior.
11. Unauth lookahead invariant (`I11`) must constrain parse window.
12. Hardened allowlist invariant (`I12`) rejects non-allowlisted deviations.

## 4) Error taxonomy (packet-specific)

1. `AuthError::WrongPass`
2. `AuthError::NoAuthRequiredButMissingState`
3. `AuthError::NoAuthGateViolation`
4. `AuthError::HelloAuthFlowViolation`
5. `AuthError::AclRuleParseViolation`
6. `AuthError::AclPermissionResolutionViolation`
7. `AuthError::AclFileTransactionalLoadViolation`
8. `AuthError::AclStartupSourceConflict`
9. `AuthError::RequirePassBridgeDrift`
10. `AuthError::AclLogContractViolation`
11. `AuthError::UnauthLookaheadViolation`
12. `AuthError::HardenedDeviationRejected`

Each error maps directly to the `auth.*` `reason_code` surface in packet artifacts.

## 5) Staged implementation sequence (risk-minimizing)

1. **Stage D1**: auth-state primitives + default-user bootstrap semantics (`C01`, `C02`)
2. **Stage D2**: noauth admission gate and command exception handling (`C05`, `T03`)
3. **Stage D3**: `AUTH` + `HELLO ... AUTH` command-path integration (`C03`, `C04`)
4. **Stage D4**: ACL selector grammar + user mutation parser skeleton (`C06`, `T05`)
5. **Stage D5**: ACL permission reducer + deny-path (`-NOPERM`) + reason index (`C07`, `C08`)
6. **Stage D6**: ACL load/save transactional + atomic persistence behavior (`C09`, `C14`)
7. **Stage D7**: startup source exclusivity + `requirepass` bridge wiring (`C10`, `C11`)
8. **Stage D8**: ACL log grouping/maxlen + unauth lookahead safeguards (`C12`, `C13`)
9. **Stage D9**: hardened allowlist enforcement + conformance adversarial sweep (`C15`, `T11`)

## 6) Unit/property test matrix

| Test ID | Contract rows | Threat IDs | Type | Expected result |
|---|---|---|---|---|
| `FR-P2C-004-U001` | `C01` | `T02` | unit | bootstrap auth state parity |
| `FR-P2C-004-U002` | `C02` | - | unit | auth success state transition |
| `FR-P2C-004-U003` | `C03` | `T01` | adversarial unit | wrongpass fail + deny-log path |
| `FR-P2C-004-U004` | `C04` | `T04` | unit | HELLO/AUTH early-fail semantics |
| `FR-P2C-004-U005` | `C05` | `T03` | adversarial unit | noauth gate precedence |
| `FR-P2C-004-U006` | `C07`, `C08` | `T06` | unit | selector deny and NOPERM parity |
| `FR-P2C-004-U007` | `C06` | `T05` | adversarial unit | ACL grammar fail-closed |
| `FR-P2C-004-U008` | `C09` | `T07` | adversarial unit | ACL load transactional rollback |
| `FR-P2C-004-U009` | `C10` | `T08` | unit | startup source conflict fail-closed |
| `FR-P2C-004-U010` | `C11` | - | unit | requirepass bridge parity |
| `FR-P2C-004-U011` | `C12` | `T09` | integration unit | ACL log grouping/maxlen semantics |
| `FR-P2C-004-U012` | `C13` | `T10` | adversarial unit | unauth lookahead bound enforcement |
| `FR-P2C-004-U013` | `C14` | - | unit | ACL save atomicity contract |
| `FR-P2C-004-U014` | `C15` | `T11` | policy unit | hardened non-allowlisted reject |

## 7) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-004-E001` | `C01` | `T02` | default-user bootstrap parity |
| `FR-P2C-004-E002` | `C02` | - | auth success flow parity |
| `FR-P2C-004-E003` | `C03` | `T01` | wrongpass rejection + no promotion |
| `FR-P2C-004-E004` | `C04` | `T04` | HELLO AUTH success/failure parity |
| `FR-P2C-004-E005` | `C05` | `T03` | noauth gate enforced pre-dispatch |
| `FR-P2C-004-E006` | `C07`, `C08` | `T06` | selector deny + NOPERM parity |
| `FR-P2C-004-E007` | `C06` | `T05` | malformed ACL rule rejection |
| `FR-P2C-004-E008` | `C09` | `T07` | ACL LOAD rollback on invalid line |
| `FR-P2C-004-E009` | `C10` | `T08` | startup source ambiguity hard-fail |
| `FR-P2C-004-E010` | `C11` | - | requirepass bridge output parity |
| `FR-P2C-004-E011` | `C12` | `T09` | ACL LOG grouping/maxlen behavior |
| `FR-P2C-004-E012` | `C13` | `T10` | unauth lookahead abuse resistance |
| `FR-P2C-004-E013` | `C14` | - | ACL SAVE atomic durability behavior |
| `FR-P2C-004-E014` | `C15` | `T11` | hardened non-allowlisted reject path |

## 8) Structured logging boundary interface

Auth boundaries (`auth_state`, `admission_gate`, `auth_cmd`, `hello_cmd`,
`acl_enforce`, `acl_source_policy`, `acl_load_save`, `acl_log`) must emit:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 9) Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-004-CLAIM-04` |
| `evidence_id` | `FR-P2C-004-EVID-PLAN-001` |
| Hotspot evidence | `D2`, `D5`, `D7` (admission gate, permission reducer, source policy) |
| Mapped graveyard section IDs | `AG-SEC-11`, `AG-DET-04`, `AG-CONF-02` |
| Baseline comparator | Legacy Redis auth/ACL integration path |
| EV score | `2.7` |
| Priority tier | `S` |
| Adoption wedge | Implement noauth+AUTH core first, then ACL admin/load, then hardened policy |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` (allowlist only) |
| Deterministic exhaustion behavior | Hardened budget exhaustion => strict-equivalent fail-closed + `auth.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_004`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004_HARDENED` |

## 10) Expected-loss decision model

States:

- `S0`: implementation preserves contract behavior
- `S1`: bounded hardened condition requiring controlled defense
- `S2`: unsafe auth/ACL drift condition

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

- `P(S1|e)` from bounded parser/compatibility anomaly rates.
- `P(S2|e)` from noauth bypass, permission mismatch, or startup policy drift signals.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive windows with calibration breach or critical-row drift (`C05`, `C09`, `C10`, `C15`).
- Fallback behavior: pause stage advancement and force strict fail-closed validation mode.

## 11) One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-004-H1`: replace repeated string-based runtime special-command routing checks (`AUTH`, `HELLO`, `ASKING`, `READONLY`, `READWRITE`, `CLUSTER`) with a length-bucketed byte classifier in `crates/fr-runtime/src/lib.rs`.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-004/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-004/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-004/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-004/isomorphism_report.md`

## 12) Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-004/env.json`
- `artifacts/phase2c/FR-P2C-004/manifest.json`
- `artifacts/phase2c/FR-P2C-004/repro.lock`
- `artifacts/phase2c/FR-P2C-004/LEGAL.md` (required if IP/provenance risk is plausible)

## 13) Execution commands (local/CI)

Use remote offload for CPU-intensive validation:

```bash
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_004
rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004
rch exec -- cargo fmt --check
```

## 14) Sequencing boundary notes

- This bead defines architecture and sequencing only.
- Behavior-changing ACL/auth implementation proceeds in `bd-2wb.15.5+`.
- Any deferred semantics remain tied to follow-up packet beads and conformance obligations.
