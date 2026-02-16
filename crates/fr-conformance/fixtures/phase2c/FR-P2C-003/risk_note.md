# FR-P2C-003 Risk Note

Packet: `FR-P2C-003`  
Subsystem: Command dispatch core  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-003/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-003/contract_table.md`

## Compatibility envelope

- `strict` mode: preserve Redis-observable dispatch behavior (lookup, arity,
  admission ordering, errors, side effects, propagation decisions) for the
  packet scope.
- `hardened` mode: allow only bounded defensive controls that do not alter
  outward reply/order/side-effect contracts.
- Unknown or non-allowlisted behavior differences must be `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-003-T01` | Parser abuse | Malformed multibulk lengths / malformed bulk lengths | `C01`, `C12` | Deterministic parse reject; no partial execution | Same | `FR-P2C-003-U012` | `FR-P2C-003-E012` | `dispatch.malformed_length_acceptance` | Critical |
| `FR-P2C-003-T02` | Protocol confusion | Non-empty inline payload on master link | `C13` | Reject master inline protocol payload deterministically | Same | `FR-P2C-003-U013` | `FR-P2C-003-E013` | `dispatch.master_inline_protocol_violation` | Critical |
| `FR-P2C-003-T03` | Admission bypass | Unauthenticated command reaches dispatch path | `C08` | `NOAUTH` emitted before dispatch execution | Same | `FR-P2C-003-U008` | `FR-P2C-003-E008` | `dispatch.noauth_gate_order_violation` | Critical |
| `FR-P2C-003-T04` | Authorization drift | ACL denial path executes command or emits wrong error family | `C09` | `NOPERM` with no side effects | Same | `FR-P2C-003-U009` | `FR-P2C-003-E009` | `dispatch.acl_denial_contract_violation` | Critical |
| `FR-P2C-003-T05` | Command confusion | Unknown command/subcommand mapped to unstable error family | `C05`, `C06` | Deterministic unknown-command/subcommand errors | Same | `FR-P2C-003-U005`, `FR-P2C-003-U006` | `FR-P2C-003-E005`, `FR-P2C-003-E006` | `dispatch.unknown_command_error_mismatch`, `dispatch.unknown_subcommand_error_mismatch` | High |
| `FR-P2C-003-T06` | Arity side-effect leak | Wrong-arity command mutates state before reject | `C07` | Reject before side effects | Same | `FR-P2C-003-U007` | `FR-P2C-003-E007` | `dispatch.wrong_arity_error_mismatch` | High |
| `FR-P2C-003-T07` | Query buffer amplification | Oversized unauthenticated stream bypasses lookahead/buffer guards | `C14` | Deterministic bounded reject behavior | Same (plus bounded diagnostics) | `FR-P2C-003-U014` | `FR-P2C-003-E014` | `dispatch.unauth_buffer_guard_violation` | Critical |
| `FR-P2C-003-T08` | Security probe bypass | HTTP-style probes (`host:`, `post`) treated as normal commands | `C11` | Security-warning reject path | Same | `FR-P2C-003-U011` | `FR-P2C-003-E011` | `dispatch.security_probe_not_rejected` | High |
| `FR-P2C-003-T09` | Transaction semantic drift | MULTI queue path executes eagerly | `C10` | Queue semantics preserved; no immediate exec | Same | `FR-P2C-003-U010` | `FR-P2C-003-E010` | `dispatch.multi_queue_semantics_mismatch` | High |
| `FR-P2C-003-T10` | Rename compatibility drift | Renamed-command fallback fails | `C15` | Original-name fallback remains deterministic | Same | `FR-P2C-003-U015` | `FR-P2C-003-E015` | `dispatch.rename_lookup_fallback_mismatch` | Medium |
| `FR-P2C-003-T11` | Cluster-route mismatch | Slot mismatch fails to redirect and executes locally | `C16` | Deterministic redirection, no local mutation | Same | `FR-P2C-003-U016` | `FR-P2C-003-E016` | `dispatch.cluster_redirection_contract_violation` | High |
| `FR-P2C-003-T12` | Durability ordering drift | Dirty/flag propagation decision diverges | `C17` | Propagation decision matches contract matrix | Same | `FR-P2C-003-U017` | `FR-P2C-003-E017` | `dispatch.propagation_decision_mismatch` | Critical |

## Fail-closed rules

1. Any malformed length/frame ambiguity rejects before dispatch (`C12`).
2. Any unauthenticated non-exempt command rejects before execution (`C08`).
3. Any non-allowlisted hardened behavior change from strict semantics is rejected.
4. Any propagation/redirection decision inconsistency on critical paths blocks packet promotion.

## Audit-log requirements

All threat detections/rejections/recoveries must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-003`
- `mode`
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

## Expected-loss decision model

### States

- `S0`: valid command and policy-allowed dispatch
- `S1`: parser/admission anomaly
- `S2`: authorization/ACL denial condition
- `S3`: propagation or redirection critical-path mismatch

### Actions

- `A0`: execute
- `A1`: deterministic reject
- `A2`: bounded hardened defense with evidence
- `A3`: fail-closed block

### Loss matrix (lower is better)

| State \\ Action | `A0` | `A1` | `A2` | `A3` |
|---|---:|---:|---:|---:|
| `S0` | 0 | 5 | 2 | 8 |
| `S1` | 11 | 0 | 2 | 5 |
| `S2` | 12 | 0 | 2 | 5 |
| `S3` | 10 | 4 | 3 | 1 |

Decision policy:

- if posterior(`S3`) >= `0.20`, enforce `A3` fail-closed;
- else if posterior(`S1`) + posterior(`S2`) >= `0.35`, use `A1`;
- otherwise use `A0`.

## Calibration and fallback trigger

- Calibration metric: Brier score target `<= 0.12` for allow/reject forecasts.
- Fallback trigger: two consecutive calibration breaches, or any drift on critical rows `C08`, `C12`, `C17`.
- Fallback action: disable non-allowlisted hardened deviations and force strict fail-closed behavior.

## Replay commands

- `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_003`
- `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_003`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_003`
- `rch exec -- cargo test -p fr-conformance -- --nocapture phase2c`

## Residual risks

- Generated command metadata/registry parity (`commands.def`-style) is not yet implemented in Rust dispatch core.
- ACL reducer and full transaction queue path are still pending implementation beads.
- Cluster redirection and propagation invariants remain high risk until packet-specific differential fixtures are landed.
