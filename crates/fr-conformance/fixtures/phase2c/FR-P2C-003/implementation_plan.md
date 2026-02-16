# FR-P2C-003 Rust Implementation Plan

Packet: `FR-P2C-003`  
Scope: Command dispatch core parity  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible command dispatch behavior for FR-P2C-003 with:

- deterministic parse-to-dispatch flow across normal and adversarial paths,
- explicit strict/hardened boundaries and fail-closed rejection rules,
- canonical error-family parity for unknown/arity/noauth/noperm classes,
- traceable unit/e2e evidence for every contract/threat row.

## 2) Module boundary skeleton

### `crates/fr-protocol` (ingest boundary)

1. Preserve deterministic parse errors for malformed multibulk/bulk lengths.
2. Expose parser outcomes needed by dispatch admission gates.
3. Keep unsupported protocol surface fail-closed unless explicitly allowlisted.

### `crates/fr-command` (lookup + dispatch core)

1. Introduce command registry abstraction (table-driven or equivalent) to replace long branch chains where feasible.
2. Normalize command lookup, subcommand lookup, and arity checks under a single deterministic contract surface.
3. Preserve canonical unknown-command and wrong-arity error families.

### `crates/fr-runtime` (admission/policy/exec envelope)

1. Preserve noauth-before-dispatch ordering.
2. Add ACL-denial integration seam (even if initially stubbed fail-closed for unimplemented portions).
3. Add transaction queue seam for MULTI semantics and command execution staging.
4. Add cluster redirection decision seam (with deterministic fail-closed behavior while incomplete).
5. Define propagation decision seam for dirty/force/prevent semantics.

### `crates/fr-config` (policy seam)

1. Maintain hardened deviation allowlist for dispatch packet.
2. Reject non-allowlisted hardened behavior changes with deterministic reason codes.

### `crates/fr-conformance` (verification seam)

1. Add packet-specific FR-P2C-003 fixtures for normal/edge/adversarial rows.
2. Bind each fixture to contract row IDs and reason codes.
3. Ensure structured-log emissions carry replay-complete metadata.

## 3) Data model invariants

1. `I01-I04`: parser + lookup determinism (including subcommand and error-family stability).
2. `I05-I09`: admission ordering, arity/noauth/acl/multi semantics.
3. `I10-I13`: resource guard, rename fallback, cluster redirect, propagation decision determinism.
4. `I14`: non-allowlisted hardened deviations fail closed.

## 4) Error taxonomy (packet-specific)

1. `dispatch.unknown_command_error_mismatch`
2. `dispatch.unknown_subcommand_error_mismatch`
3. `dispatch.wrong_arity_error_mismatch`
4. `dispatch.noauth_gate_order_violation`
5. `dispatch.acl_denial_contract_violation`
6. `dispatch.malformed_length_acceptance`
7. `dispatch.master_inline_protocol_violation`
8. `dispatch.unauth_buffer_guard_violation`
9. `dispatch.security_probe_not_rejected`
10. `dispatch.rename_lookup_fallback_mismatch`
11. `dispatch.cluster_redirection_contract_violation`
12. `dispatch.propagation_decision_mismatch`
13. `dispatch.hardened_budget_exhausted_failclosed`

## 5) Staged implementation sequence

1. **Stage D1**: lock canonical error-family parity for unknown/subcommand/arity (`C05-C07`).
2. **Stage D2**: lock admission ordering (`NOAUTH` before dispatch) and no-side-effect guarantees (`C08`).
3. **Stage D3**: implement/mirror parser-edge reject behavior for malformed lengths and master-inline rejects (`C12-C13`).
4. **Stage D4**: introduce dispatch registry abstraction + subcommand lookup seam (`C03-C04`).
5. **Stage D5**: add ACL deny seam + deterministic NOPERM behavior (`C09`).
6. **Stage D6**: add MULTI queue seam and compatibility tests (`C10`).
7. **Stage D7**: add cluster redirection + rename fallback seams (`C15-C16`).
8. **Stage D8**: add propagation decision seam and related invariants (`C17`).
9. **Stage D9**: run packet-specific conformance suite in strict/hardened modes with log-contract checks.

## 6) Unit/property test matrix

| Test ID | Contract rows | Threat IDs | Type | Expected result |
|---|---|---|---|---|
| `FR-P2C-003-U001` | `C01` | `T01` | unit | multibulk parse->dispatch determinism |
| `FR-P2C-003-U002` | `C02` | - | unit | inline vs multibulk semantic equivalence |
| `FR-P2C-003-U003` | `C03` | - | unit | known-command lookup determinism |
| `FR-P2C-003-U004` | `C04` | - | unit | one-level subcommand resolution parity |
| `FR-P2C-003-U005` | `C05` | `T05` | unit | unknown-command error-family parity |
| `FR-P2C-003-U006` | `C06` | `T05` | unit | unknown-subcommand error-family parity |
| `FR-P2C-003-U007` | `C07` | `T06` | unit | wrong-arity reject before side effects |
| `FR-P2C-003-U008` | `C08` | `T03` | unit | noauth gate ordering |
| `FR-P2C-003-U009` | `C09` | `T04` | unit | ACL deny semantics |
| `FR-P2C-003-U010` | `C10` | `T09` | unit | MULTI queue semantics |
| `FR-P2C-003-U011` | `C11` | `T08` | adversarial unit | security probe reject path |
| `FR-P2C-003-U012` | `C12` | `T01` | adversarial unit | malformed length fail-closed behavior |
| `FR-P2C-003-U013` | `C13` | `T02` | adversarial unit | master-inline fail-closed behavior |
| `FR-P2C-003-U014` | `C14` | `T07` | adversarial unit | unauth buffer guard behavior |
| `FR-P2C-003-U015` | `C15` | `T10` | unit | rename fallback lookup behavior |
| `FR-P2C-003-U016` | `C16` | `T11` | unit | cluster redirection semantics |
| `FR-P2C-003-U017` | `C17` | `T12` | unit | propagation decision determinism |
| `FR-P2C-003-U018` | `I14` | - | policy unit | non-allowlisted hardened drift rejected |

## 7) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-003-E001` | `C01` | `T01` | multibulk dispatch parity |
| `FR-P2C-003-E002` | `C02` | - | inline dispatch parity |
| `FR-P2C-003-E003` | `C03` | - | known-command lookup parity |
| `FR-P2C-003-E004` | `C04` | - | subcommand resolution parity |
| `FR-P2C-003-E005` | `C05` | `T05` | unknown-command error parity |
| `FR-P2C-003-E006` | `C06` | `T05` | unknown-subcommand error parity |
| `FR-P2C-003-E007` | `C07` | `T06` | wrong-arity reject parity |
| `FR-P2C-003-E008` | `C08` | `T03` | noauth gate ordering parity |
| `FR-P2C-003-E009` | `C09` | `T04` | ACL denial parity |
| `FR-P2C-003-E010` | `C10` | `T09` | MULTI queue parity |
| `FR-P2C-003-E011` | `C11` | `T08` | security probe handling parity |
| `FR-P2C-003-E012` | `C12` | `T01` | malformed frame fail-closed parity |
| `FR-P2C-003-E013` | `C13` | `T02` | master-inline reject parity |
| `FR-P2C-003-E014` | `C14` | `T07` | unauth pressure guard parity |
| `FR-P2C-003-E015` | `C15` | `T10` | rename fallback parity |
| `FR-P2C-003-E016` | `C16` | `T11` | cluster redirection parity |
| `FR-P2C-003-E017` | `C17` | `T12` | propagation decision parity |
| `FR-P2C-003-E018` | `I14` | - | hardened non-allowlisted reject parity |

## 8) Structured logging boundary interface

Dispatch boundary events (`parse_in`, `lookup`, `arity_gate`, `admission_gate`,
`acl_gate`, `dispatch_exec`, `propagation_decision`) must emit:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 9) Execution commands (local/CI)

Use remote offload for CPU-intensive validation:

```bash
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_003
rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_003
rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_003
rch exec -- cargo test -p fr-conformance -- --nocapture phase2c
rch exec -- cargo fmt --check
```

## 10) Sequencing boundary notes

- This bead defines architecture + execution sequencing only.
- Behavior-changing implementation lands in downstream beads (`bd-2wb.14.5+`).
- Packet promotion requires strict/hardened conformance evidence with zero unresolved critical-row drift.
