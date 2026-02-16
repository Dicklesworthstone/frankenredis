# FR-P2C-003 Contract Table

Packet: `FR-P2C-003`  
Subsystem: Command dispatch core  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-003/legacy_anchor_map.md`

## Contract row schema (normative)

Each row defines:

- `trigger`: deterministic dispatch event.
- `preconditions`: required parser/runtime state before evaluation.
- `strict_contract`: Redis-observable behavior that must match legacy semantics.
- `hardened_contract`: bounded defensive behavior that preserves outward API contract.
- `fail_closed_boundary`: mandatory hard-failure edge.
- `unit_trace` / `e2e_trace`: required verification linkage.
- `reason_codes`: deterministic diagnostics required on mismatch.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-003-C01` | Parse valid RESP multibulk command and dispatch | Complete RESP frame, command exists | argv materialization order and command dispatch identity are deterministic and side-effect equivalent to legacy path. | Same outward behavior; extra diagnostics allowed only when behavior-identical. | Parsed argv order drift or dispatch target mismatch. | `FR-P2C-003-U001` | `FR-P2C-003-E001` | `dispatch.multibulk_parse_dispatch_mismatch` |
| `FR-P2C-003-C02` | Parse valid inline command (non-master client) | Inline mode active, line is complete | Inline tokenization yields same argv semantics as equivalent multibulk command. | Same. | Inline parse accepted with divergent argv semantics. | `FR-P2C-003-U002` | `FR-P2C-003-E002` | `dispatch.inline_parse_semantics_mismatch` |
| `FR-P2C-003-C03` | Lookup known command | Command name resolves in active command table | Lookup is deterministic and binds expected command metadata for downstream checks. | Same. | Ambiguous or unstable command lookup result. | `FR-P2C-003-U003` | `FR-P2C-003-E003` | `dispatch.lookup_known_command_mismatch` |
| `FR-P2C-003-C04` | Lookup command with one-level subcommand | Container command and subcommand token present | One-level subcommand resolution follows container lookup rules; strict arity for strict-lookup mode remains deterministic. | Same. | Subcommand path resolves differently or silently falls through to base command. | `FR-P2C-003-U004` | `FR-P2C-003-E004` | `dispatch.subcommand_resolution_mismatch` |
| `FR-P2C-003-C05` | Unknown command name | Command not found | Error family: `ERR unknown command '<cmd>'` with deterministic args-preview behavior and newline-safe sanitization. | Same outward error; bounded telemetry allowed. | Unknown command accepted or mapped to incorrect error family. | `FR-P2C-003-U005` | `FR-P2C-003-E005` | `dispatch.unknown_command_error_mismatch` |
| `FR-P2C-003-C06` | Missing/unknown subcommand on container command | Base command exists and has subcommands | Error differentiates missing subcommand vs unknown subcommand and preserves deterministic help guidance semantics. | Same. | Missing/unknown subcommand collapsed into generic unrelated error family. | `FR-P2C-003-U006` | `FR-P2C-003-E006` | `dispatch.unknown_subcommand_error_mismatch` |
| `FR-P2C-003-C07` | Wrong arity for known command | Command exists, argument count invalid | Canonical arity reject occurs before side effects with deterministic error text family. | Same. | Command executes or mutates state after arity mismatch. | `FR-P2C-003-U007` | `FR-P2C-003-E007` | `dispatch.wrong_arity_error_mismatch` |
| `FR-P2C-003-C08` | Auth-required client attempts non-allowed command | Client unauthenticated and command lacks noauth exemption | `NOAUTH` gate runs before normal dispatch execution path. | Same. | Dispatch proceeds before auth gate or returns non-`NOAUTH` family. | `FR-P2C-003-U008` | `FR-P2C-003-E008` | `dispatch.noauth_gate_order_violation` |
| `FR-P2C-003-C09` | ACL permission check denies command | Client authenticated; ACL reducer denies command/key/channel | Deterministic `NOPERM` response and denial metadata path with no command side effects. | Same outward behavior; bounded forensic enrichment allowed. | ACL-denied command executes or returns non-`NOPERM` family. | `FR-P2C-003-U009` | `FR-P2C-003-E009` | `dispatch.acl_denial_contract_violation` |
| `FR-P2C-003-C10` | MULTI context receives non-control command | Client in transaction context | Command is queued (not executed immediately) and emits deterministic queued semantics. | Same. | Command executes immediately in MULTI queue path. | `FR-P2C-003-U010` | `FR-P2C-003-E010` | `dispatch.multi_queue_semantics_mismatch` |
| `FR-P2C-003-C11` | Suspicious probe command (`host:`/`post`) | Unknown command path reached | Security probe path rejects and terminates connection per policy. | Same; bounded diagnostics may be added. | Probe command continues through normal unknown-command response path. | `FR-P2C-003-U011` | `FR-P2C-003-E011` | `dispatch.security_probe_not_rejected` |
| `FR-P2C-003-C12` | Malformed multibulk or bulk length encoding | RESP parser receives invalid count/length | Deterministic parse rejection with no partial execution. | Same. | Malformed frame partially executes or is accepted. | `FR-P2C-003-U012` | `FR-P2C-003-E012` | `dispatch.malformed_length_acceptance` |
| `FR-P2C-003-C13` | Master sends non-empty inline command payload | Client flagged as master | Dispatch path rejects master inline command payload deterministically. | Same. | Master inline command is accepted and executed. | `FR-P2C-003-U013` | `FR-P2C-003-E013` | `dispatch.master_inline_protocol_violation` |
| `FR-P2C-003-C14` | Unauthenticated oversized request stream | Client unauthenticated, high input pressure | Lookahead/query buffer safety policy prevents speculative parse amplification and preserves deterministic rejection behavior. | Same outward behavior; bounded resource clamp diagnostics allowed. | Unauthenticated path parses/speculatively executes beyond safety policy. | `FR-P2C-003-U014` | `FR-P2C-003-E014` | `dispatch.unauth_buffer_guard_violation` |
| `FR-P2C-003-C15` | Renamed command lookup fallback | Runtime command rename applied | `lookupCommandOrOriginal`-equivalent behavior preserves executable mapping when command vector is rewritten. | Same. | Renamed command loses lookup mapping or resolves to wrong command. | `FR-P2C-003-U015` | `FR-P2C-003-E015` | `dispatch.rename_lookup_fallback_mismatch` |
| `FR-P2C-003-C16` | Cluster key-slot ownership mismatch | Cluster mode active and command has key specs | Dispatch emits deterministic redirection path and avoids local key mutation. | Same. | Redirection mismatch or local execution on non-owned slot. | `FR-P2C-003-U016` | `FR-P2C-003-E016` | `dispatch.cluster_redirection_contract_violation` |
| `FR-P2C-003-C17` | Dirty write command with propagation enabled | `call()`-equivalent execution context with propagation flags | AOF/replication propagation decision follows dirty bit plus force/prevent flags deterministically. | Same; bounded telemetry allowed. | Dirty/flag-driven propagation decision diverges from contract. | `FR-P2C-003-U017` | `FR-P2C-003-E017` | `dispatch.propagation_decision_mismatch` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-003-I01` | Multibulk parse to argv determinism | Required | Required |
| `FR-P2C-003-I02` | Inline parse semantic equivalence | Required | Required |
| `FR-P2C-003-I03` | Command lookup determinism | Required | Required |
| `FR-P2C-003-I04` | Unknown/subcommand error-family stability | Required | Required |
| `FR-P2C-003-I05` | Arity gate before side effects | Required | Required |
| `FR-P2C-003-I06` | Noauth gate ordering | Required | Required |
| `FR-P2C-003-I07` | ACL denial semantics | Required | Required |
| `FR-P2C-003-I08` | MULTI queue semantics | Required | Required |
| `FR-P2C-003-I09` | Master inline rejection | Required | Required |
| `FR-P2C-003-I10` | Unauthenticated buffer/lookahead guard | Required | Required |
| `FR-P2C-003-I11` | Rename fallback lookup stability | Required | Required |
| `FR-P2C-003-I12` | Cluster redirection semantics | Required | Required |
| `FR-P2C-003-I13` | Propagation decision determinism | Required | Required |
| `FR-P2C-003-I14` | Non-allowlisted hardened deviations | N/A (strict baseline) | Reject non-allowlisted deviations |

## Allowed hardened deviations (bounded)

- `BoundedParserDiagnostics`: richer parse/admission diagnostics without changing user-visible reply family.
- `ResourceClamp`: deterministic bounded rejection under hostile request pressure.
- `ForensicMetadataEnrichment`: additional structured-log metadata preserving reply/ordering semantics.

Non-allowlisted behavior differences are rejected and treated as `fail_closed`.

## Structured-log contract for FR-P2C-003 rows

Each contract-row verification event (pass/fail/drift) must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-003`)
- `mode` (`strict|hardened`)
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

## Replay command templates

- Unit/property: `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_003`
- Runtime dispatch: `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_003`
- Integration/E2E: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_003`
- Packet schema gate: `rch exec -- cargo test -p fr-conformance -- --nocapture phase2c`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-003-CLAIM-02` |
| `evidence_id` | `FR-P2C-003-EVID-CONTRACT-001` |
| Hotspot evidence | `C08`, `C12`, `C17` (admission ordering, malformed-frame reject, propagation core) |
| Mapped graveyard section IDs | `AG-SEC-11`, `AG-DET-04`, `AG-PIPE-03` |
| Baseline comparator | Legacy Redis dispatch pipeline (`processInputBuffer` -> `processCommand` -> `call`) |
| EV score | `2.8` |
| Priority tier | `S` |
| Adoption wedge | Enforce admission-order + malformed-frame + propagation rows first, then broaden command-surface parity |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` on explicit allowlist only |
| Deterministic exhaustion behavior | On hardened budget exhaustion, emit `dispatch.hardened_budget_exhausted_failclosed` and force strict-equivalent outcomes |
| Replay commands | `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_003`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_003` |

## Expected-loss decision model

States:

- `S0`: policy-allowed command on valid frame
- `S1`: parser/admission anomaly (malformed/oversized/inline-master mismatch)
- `S2`: authorization/policy denial path (NOAUTH/NOPERM)
- `S3`: propagation/redirection critical-path mismatch

Actions:

- `A0`: execute
- `A1`: deterministic reject (canonical error family)
- `A2`: bounded hardened defense with evidence emission
- `A3`: fail-closed block

Loss matrix:

| State \\ Action | `A0` | `A1` | `A2` | `A3` |
|---|---:|---:|---:|---:|
| `S0` | 0.0 | 5.0 | 2.5 | 8.0 |
| `S1` | 11.0 | 0.0 | 2.0 | 5.0 |
| `S2` | 12.0 | 0.0 | 2.0 | 5.0 |
| `S3` | 10.0 | 4.0 | 3.0 | 1.0 |

Posterior/evidence terms:

- `P(S1|e)`: malformed-frame rate, master-inline violations, unauth buffer pressure events.
- `P(S2|e)`: noauth/acl-denial frequency and denial consistency evidence.
- `P(S3|e)`: propagation drift and cluster redirection mismatch events.

Calibration + fallback:

- calibration target: Brier score `<= 0.12` for allow/reject forecasts;
- fallback trigger: two consecutive calibration breaches or any critical-row drift in `C08`, `C12`, `C17`;
- fallback behavior: disable non-allowlisted hardened deviations and force strict fail-closed behavior.

## One-lever extreme-optimization loop artifacts

Selected lever:

- `LEV-003-02`: canonical command lookup accelerator keyed by `(command_token_hash, argc_shape, mode)` with deterministic invalidation boundaries.

Required artifacts:

- `artifacts/phase2c/FR-P2C-003/baseline_profile.json`
- `artifacts/phase2c/FR-P2C-003/lever_selection.md`
- `artifacts/phase2c/FR-P2C-003/post_profile.json`
- `artifacts/phase2c/FR-P2C-003/isomorphism_report.md`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-003/env.json`
- `artifacts/phase2c/FR-P2C-003/manifest.json`
- `artifacts/phase2c/FR-P2C-003/repro.lock`
- `artifacts/phase2c/FR-P2C-003/LEGAL.md` (required if IP/provenance risk is detected)

## Traceability checklist

- Every contract row maps to at least one unit ID and one e2e ID.
- Every row includes deterministic reason-code bindings.
- Every row declares strict/hardened behavior and explicit fail-closed boundary.
- User-visible compatibility outcomes (reply family, side effects, ordering) are explicit on all high-risk rows.
