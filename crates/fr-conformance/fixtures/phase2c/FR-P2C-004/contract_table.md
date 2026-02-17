# FR-P2C-004 Contract Table

Packet: `FR-P2C-004`  
Subsystem: ACL and auth policy  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/legacy_anchor_map.md`

## Contract row schema (normative)

Each row defines:

- `trigger`: deterministic auth/ACL event.
- `preconditions`: required state before evaluation.
- `strict_contract`: Redis-observable behavior that must match legacy semantics.
- `hardened_contract`: bounded defensive behavior that preserves outward API contract.
- `fail_closed_boundary`: mandatory hard-failure edge.
- `unit_trace` / `e2e_trace`: required verification mapping.
- `reason_codes`: deterministic diagnostics required on mismatch.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-004-C01` | Client connection auth bootstrap | Default user loaded | If default user is `nopass` and enabled, connection starts authenticated; otherwise unauthenticated. | Same external behavior; additional diagnostics allowed. | Any state where `nopass+enabled` client starts unauthenticated or inverse. | `FR-P2C-004-U001` | `FR-P2C-004-E001` | `auth.default_nopass_bootstrap_mismatch` |
| `FR-P2C-004-C02` | `AUTH <password>` or `AUTH <user> <password>` success path | User exists, enabled, credentials valid | Reply `+OK`, authenticated state flips, user binding is updated deterministically. | Same; optional bounded telemetry only. | Success path returns non-OK or does not transition auth state. | `FR-P2C-004-U002` | `FR-P2C-004-E002` | `auth.auth_command_success_state_mismatch` |
| `FR-P2C-004-C03` | `AUTH` failure path | Invalid credentials or disabled user | Reply `-WRONGPASS ...`; auth state remains unauthenticated; denial logged as auth failure. | Same outward reply/state; richer diagnostics permitted. | Failed auth accepted as success or wrong error family emitted. | `FR-P2C-004-U003` | `FR-P2C-004-E003` | `auth.auth_command_wrongpass_response_mismatch`, `auth.auth_deny_log_missing` |
| `FR-P2C-004-C04` | `HELLO <ver> AUTH <user> <pass>` | Supported protocol version (2 or 3) | In-band auth integrated with HELLO; failed auth returns early and does not complete HELLO response. | Same; bounded diagnostics allowed. | HELLO proceeds after failed/blocked auth, or unauthenticated HELLO success without AUTH. | `FR-P2C-004-U004` | `FR-P2C-004-E004` | `auth.hello_auth_flow_semantics_mismatch`, `auth.hello_unauth_bypass` |
| `FR-P2C-004-C05` | Command execution while unauthenticated | Command lacks `CMD_NO_AUTH` exception | Server returns `-NOAUTH Authentication required.` before normal command path. | Same. | Unauthorized command execution proceeds past noauth gate. | `FR-P2C-004-U005` | `FR-P2C-004-E005` | `auth.noauth_gate_violation` |
| `FR-P2C-004-C06` | ACL selector/user rule parse (`ACL SETUSER` / load) | Rule token stream provided | Selector grammar and `ACLSetUser` semantics enforce deterministic syntax/category/firstarg constraints. | Same; bounded parser diagnostics allowed if contract-preserving. | Malformed/invalid ACL rule accepted. | `FR-P2C-004-U007` | `FR-P2C-004-E007` | `auth.acl_selector_parse_validation_mismatch` |
| `FR-P2C-004-C07` | ACL command/key/channel permission reduction | Client authenticated with user selectors | Permission decision follows selector evaluation and deterministic denial index selection. | Same outward deny/allow semantics; internal cache allowed if isomorphic. | Allow/deny differs from legacy selector reduction or unstable denial index. | `FR-P2C-004-U006` | `FR-P2C-004-E006` | `auth.command_perm_resolution_mismatch`, `auth.denied_index_mismatch` |
| `FR-P2C-004-C08` | ACL denial on command path | ACL returns `ACL_DENIED_*` | Return `-NOPERM ...` and write ACL log with reason/object/context. | Same; additional evidence fields allowed. | Denial without user-visible NOPERM or without denial log entry. | `FR-P2C-004-U011` | `FR-P2C-004-E011` | `auth.noperm_reply_contract_violation`, `auth.acl_log_contract_violation` |
| `FR-P2C-004-C09` | ACL file load (`ACL LOAD` / startup aclfile path) | ACL file present/readable | Load is transactional: any invalid line causes rollback, preserving prior ACL state. | Same; bounded metadata sanitization only when behavior-identical. | Partial ACL state activation after failed load. | `FR-P2C-004-U008` | `FR-P2C-004-E008` | `auth.acl_file_transactional_load_violation` |
| `FR-P2C-004-C10` | Startup with ACL source ambiguity | Both `aclfile` and configured users present | Startup fails closed with deterministic fatal path; service does not start. | Same (no hardened relaxation). | Startup continues with mixed ACL sources. | `FR-P2C-004-U009` | `FR-P2C-004-E009` | `auth.acl_startup_source_conflict_not_failclosed` |
| `FR-P2C-004-C11` | `requirepass` config update bridge | Default user exists | `requirepass` mutates default ACL user (`resetpass` + set password or `nopass` when empty). | Same; bounded diagnostics allowed. | Config update diverges from default-user ACL state. | `FR-P2C-004-U010` | `FR-P2C-004-E010` | `auth.requirepass_bridge_drift` |
| `FR-P2C-004-C12` | ACL log ingestion under repeated denials | `acllog_max_len` configured | Equivalent entries are grouped; count/timestamps/entry identity and trimming semantics remain deterministic. | Same; extra telemetry allowed. | Log overflows unboundedly or grouping breaks identity semantics. | `FR-P2C-004-U011` | `FR-P2C-004-E011` | `auth.acl_log_grouping_contract_violation` |
| `FR-P2C-004-C13` | Unauthenticated input-buffer processing | Client not authenticated | Lookahead constrained to 1 command-equivalent unit to prevent post-auth parsing hazards and memory amplification. | Same externally visible behavior; bounded defensive rejection permitted. | Multi-command speculative parse/execute while unauthenticated. | `FR-P2C-004-U012` | `FR-P2C-004-E012` | `auth.unauthed_lookahead_policy_violation` |
| `FR-P2C-004-C14` | ACL file save path | ACL user set loaded | Save is atomic temp-write + fsync + rename + dir-fsync; no torn ACL persistence. | Same. | ACL save reports success without durability sequence completion. | `FR-P2C-004-U013` | `FR-P2C-004-E013` | `auth.acl_save_atomicity_violation` |
| `FR-P2C-004-C15` | Hardened-mode candidate deviation outside allowlist | Mode=`hardened`, deviation unresolved | Strict-equivalent fail-closed baseline unless deviation category is explicitly allowlisted. | Only allowlisted bounded defenses may proceed and must emit policy evidence. | Non-allowlisted deviation changes outward auth/ACL behavior. | `FR-P2C-004-U014` | `FR-P2C-004-E014` | `auth.hardened_nonallowlisted_rejected`, `auth.hardened_policy_violation` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-004-I01` | Auth bootstrap by default user flags | Required | Required |
| `FR-P2C-004-I02` | `AUTH` success/failure reply and state transitions | Required | Required |
| `FR-P2C-004-I03` | `HELLO ... AUTH` early-fail semantics | Required | Required |
| `FR-P2C-004-I04` | Noauth gate before command execution | Required | Required |
| `FR-P2C-004-I05` | Selector/user grammar strictness | Required | Required |
| `FR-P2C-004-I06` | Selector reduction deterministic deny index | Required | Required |
| `FR-P2C-004-I07` | ACL file transactional rollback | Required | Required |
| `FR-P2C-004-I08` | Startup ACL-source ambiguity fail-closed | Required | Required |
| `FR-P2C-004-I09` | `requirepass` compatibility bridge | Required | Required |
| `FR-P2C-004-I10` | ACL log grouping/maxlen semantics | Required | Required |
| `FR-P2C-004-I11` | Unauthenticated lookahead safety | Required | Required |
| `FR-P2C-004-I12` | Non-allowlisted hardened deviations | N/A (strict fail-closed baseline) | Reject non-allowlisted deviations |

## Allowed hardened deviations (bounded)

- `BoundedParserDiagnostics`: additional parse diagnostics for ACL rule failures without behavior drift.
- `MetadataSanitization`: bounded metadata normalization only when equivalent to strict acceptance/rejection.
- `ResourceClamp`: bounded unauthenticated input clamp preserving noauth/parse contract.

Non-allowlisted behavior differences are rejected and treated as `fail_closed`.

## Structured-log contract for FR-P2C-004 rows

Each contract-row verification result (pass/fail and divergence checks) must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-004`)
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

- Unit/property: `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_004`
- Integration/E2E: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004`
- Strict-mode sweep: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004_STRICT`
- Hardened-mode sweep: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004_HARDENED`

## Implemented final evidence pack (bd-2wb.15.9)

- Packet final manifest + parity gate authored in `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/{fixture_manifest.json,parity_gate.yaml}`.
- Packet parity report finalized in `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/parity_report.json` with:
  - `readiness=READY_FOR_IMPL`
  - `missing_mandatory_fields=[]`
  - explicit unit/differential/e2e/optimization evidence IDs.
- Durability sidecar + decode-proof artifacts finalized in:
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/parity_report.raptorq.json`
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/parity_report.decode_proof.json`
  - reason code: `raptorq.decode_verified`
  - replay command: `./scripts/run_raptorq_artifact_gate.sh --run-id local-smoke`
- Packet schema/readiness replay:
  - `rch exec -- cargo run -p fr-conformance --bin phase2c_schema_gate -- crates/fr-conformance/fixtures/phase2c/FR-P2C-004`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-004-CLAIM-02` |
| `evidence_id` | `FR-P2C-004-EVID-CONTRACT-001` |
| Hotspot evidence | `C05`, `C07`, `C10` (noauth gate, selector reduction, startup ambiguity fail-closed) |
| Mapped graveyard section IDs | `AG-SEC-11`, `AG-DET-04`, `AG-CONF-02` |
| Baseline comparator | Legacy Redis ACL/auth control path (`acl.c`/`server.c`/`networking.c`) |
| EV score | `2.9` |
| Priority tier | `S` |
| Adoption wedge | Land noauth gate + deterministic reason codes first, then selector model + rollback loader |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` only on explicit allowlist |
| Deterministic exhaustion behavior | On hardened budget exhaustion, emit `auth.hardened_budget_exhausted_failclosed` and force strict-equivalent outcome |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_004`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004_HARDENED` |

## Expected-loss decision model

States:

- `S0`: authenticated and ACL-allowed command
- `S1`: unauthenticated command path entry
- `S2`: authenticated but ACL-denied command/key/channel
- `S3`: configuration/source ambiguity or ACL-load inconsistency

Actions:

- `A0`: allow
- `A1`: deterministic reject (`-NOAUTH`/`-NOPERM`/`-WRONGPASS`)
- `A2`: bounded hardened defense
- `A3`: fail-closed halt/abort

Loss matrix:

| State \ Action | `A0` | `A1` | `A2` | `A3` |
|---|---:|---:|---:|---:|
| `S0` | 0.0 | 4.0 | 2.0 | 7.0 |
| `S1` | 11.0 | 0.0 | 2.0 | 6.0 |
| `S2` | 10.0 | 0.0 | 1.5 | 5.0 |
| `S3` | 12.0 | 4.0 | 3.0 | 0.0 |

Posterior/evidence terms:

- `P(S1|e)`: derived from unauthenticated-request rate and noauth-gate telemetry.
- `P(S2|e)`: derived from ACL deny reason distribution and selector mismatch evidence.
- `P(S3|e)`: derived from startup source-conflict events and ACL load transactional failures.

Calibration + fallback:

- Calibration target: Brier score `<= 0.12`.
- Fallback trigger: calibration breach across 2 consecutive windows or `P(S3|e) >= 0.20`.
- Trigger behavior: disable hardened deviations for packet scope and force strict fail-closed.

## One-lever extreme-optimization loop artifacts

Selected lever:

- `LEV-004-H1`: replace repeated string-based runtime special-command routing checks (`AUTH`, `HELLO`, `ASKING`, `READONLY`, `READWRITE`, `CLUSTER`) with a length-bucketed byte classifier in `crates/fr-runtime/src/lib.rs`.

Required artifacts:

- Baseline/profile: `artifacts/phase2c/FR-P2C-004/baseline_profile.json`
- Lever decision note: `artifacts/phase2c/FR-P2C-004/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-004/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-004/isomorphism_report.md`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-004/env.json`
- `artifacts/phase2c/FR-P2C-004/manifest.json`
- `artifacts/phase2c/FR-P2C-004/repro.lock`
- `artifacts/phase2c/FR-P2C-004/LEGAL.md` (required if IP/provenance risk is found)

## Traceability checklist

- Every contract row maps to at least one unit ID and one e2e ID.
- Every contract row declares deterministic `reason_code` values.
- Every contract row includes explicit strict/hardened expectations and fail-closed boundary.
- User-visible compatibility outcomes are explicit for reply/state/order behavior.
