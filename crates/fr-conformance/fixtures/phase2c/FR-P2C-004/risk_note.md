# FR-P2C-004 Risk Note

Packet: `FR-P2C-004`  
Subsystem: ACL and auth policy  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/contract_table.md`

## Compatibility envelope

- `strict` mode: preserve Redis-observable auth/ACL replies, side effects, and
  ordering (`AUTH`, `HELLO ... AUTH`, `-NOAUTH`, `-NOPERM`, ACL admin semantics).
- `hardened` mode: permit only bounded defenses
  (`BoundedParserDiagnostics`, `MetadataSanitization`, `ResourceClamp`) with no
  outward contract drift.
- Unknown or non-allowlisted behavior is `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-004-T01` | Credential attack | Password brute force / wrong credentials against `AUTH` | `C02`, `C03`, `I02` | Reject invalid auth with `-WRONGPASS`; no auth-state promotion | Same; richer diagnostics allowed | `FR-P2C-004-U003` | `FR-P2C-004-E003` | `auth.auth_command_wrongpass_response_mismatch` | Critical |
| `FR-P2C-004-T02` | Bootstrap auth confusion | Default-user flag drift (`nopass`, `disabled`) causes incorrect initial auth state | `C01`, `I01` | Deterministic bootstrap from default-user flags | Same | `FR-P2C-004-U001` | `FR-P2C-004-E001` | `auth.default_nopass_bootstrap_mismatch` | High |
| `FR-P2C-004-T03` | Admission-control bypass | Non-`CMD_NO_AUTH` command executes while unauthenticated | `C05`, `I04` | Return `-NOAUTH` before command execution | Same | `FR-P2C-004-U005` | `FR-P2C-004-E005` | `auth.noauth_gate_violation` | Critical |
| `FR-P2C-004-T04` | Protocol/auth confusion | `HELLO ... AUTH` proceeds after failed/blocked auth or bypasses auth requirement | `C04`, `I03` | Fail auth early, no protocol negotiation success on failure | Same | `FR-P2C-004-U004` | `FR-P2C-004-E004` | `auth.hello_auth_flow_semantics_mismatch`, `auth.hello_unauth_bypass` | High |
| `FR-P2C-004-T05` | ACL rule poisoning | Malformed ACL selector/user rule accepted (`SETUSER`/load path) | `C06`, `I05` | Reject malformed rule deterministically | Same with bounded parser diagnostics | `FR-P2C-004-U007` | `FR-P2C-004-E007` | `auth.acl_selector_parse_validation_mismatch` | Critical |
| `FR-P2C-004-T06` | Authorization bypass | Selector reduction bug allows denied command/key/channel | `C07`, `C08`, `I06` | Enforce selector reduction and emit `-NOPERM` + denial log | Same | `FR-P2C-004-U006` | `FR-P2C-004-E006` | `auth.command_perm_resolution_mismatch`, `auth.noperm_reply_contract_violation` | Critical |
| `FR-P2C-004-T07` | ACL persistence tampering | ACL file load partially applies invalid ACL lines | `C09`, `I07` | Transactional rollback; old ACL state preserved | Same | `FR-P2C-004-U008` | `FR-P2C-004-E008` | `auth.acl_file_transactional_load_violation` | Critical |
| `FR-P2C-004-T08` | Config-source downgrade abuse | Service starts with mixed ACL sources (`aclfile` + configured users) | `C10`, `I08` | Startup hard-fails (fail closed) | Same | `FR-P2C-004-U009` | `FR-P2C-004-E009` | `auth.acl_startup_source_conflict_not_failclosed` | Critical |
| `FR-P2C-004-T09` | Audit/log suppression | ACL denial log grouping/trim behavior drops or corrupts forensic signal | `C12`, `I10` | Grouping/trim semantics remain bounded and deterministic | Same | `FR-P2C-004-U011` | `FR-P2C-004-E011` | `auth.acl_log_grouping_contract_violation` | High |
| `FR-P2C-004-T10` | Resource amplification | Unauthenticated input lookahead too permissive causing parse/memory pressure | `C13`, `I11` | Enforce unauth lookahead constraint | Same with bounded clamp only | `FR-P2C-004-U012` | `FR-P2C-004-E012` | `auth.unauthed_lookahead_policy_violation` | High |
| `FR-P2C-004-T11` | Policy downgrade abuse | Hardened mode applies non-allowlisted behavior deviation | `C15`, `I12` | N/A (strict fail-closed baseline) | Reject non-allowlisted deviation | `FR-P2C-004-U014` | `FR-P2C-004-E014` | `auth.hardened_nonallowlisted_rejected`, `auth.hardened_policy_violation` | Critical |

## Fail-closed rules

1. Noauth gate bypass is never recoverable (`C05`).
2. Malformed ACL selector/user rule acceptance is fatal for contract compliance (`C06`).
3. ACL file partial activation after failed load is forbidden (`C09`).
4. Mixed ACL source startup ambiguity must halt service (`C10`).
5. Non-allowlisted hardened behavior differences are rejected (`C15`).

## Audit-log requirements

All threat detections/rejections/recoveries must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-004`
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
| `claim_id` | `FR-P2C-004-CLAIM-03` |
| `evidence_id` | `FR-P2C-004-EVID-RISK-001` |
| Hotspot evidence | `T03`, `T06`, `T08` (entry-point gate, selector authorization, startup fail-closed) |
| Mapped graveyard section IDs | `AG-SEC-11`, `AG-DET-04`, `AG-CONF-02` |
| Baseline comparator | Legacy Redis ACL/auth threat surface (`acl.c` + `server.c` + `networking.c`) |
| EV score | `3.0` |
| Priority tier | `S` |
| Adoption wedge | Implement noauth gate + selector reducer + transactional ACL load before expanding command family breadth |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | Exhaustion forces strict-equivalent fail-closed and emits `auth.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_004`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004_HARDENED` |

## Expected-loss decision model

### States

- `S0`: contract-preserving auth/ACL operation
- `S1`: recoverable bounded condition (allowlisted diagnostics/clamp)
- `S2`: unsafe authorization/config drift condition

### Actions

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense with evidence emission
- `A2`: fail closed and reject/halt

### Loss matrix (lower is better)

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)`: rises with parser/compatibility anomaly telemetry without auth-state drift.
- `P(S2|e)`: rises with noauth bypass evidence, selector mismatch outcomes, and startup source conflicts.

Decision policy:

- If posterior(`S2`) `> 0.30`, enforce `A2` fail-closed.
- If posterior(`S1`) `> 0.40` and deviation is allowlisted, use `A1`.
- Otherwise use `A0`.

## Calibration and fallback trigger

- Calibration metric: false-negative rate on adversarial auth/ACL suite `< 1%`.
- Fallback trigger: unresolved strict-mode drift on critical rows (`C05`, `C06`, `C09`, `C10`, `C15`) blocks packet promotion.
- Budget exhaustion policy: if hardened budget is exhausted in two consecutive windows, revert packet execution to strict fail-closed behavior.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-004-03`: short-circuit noauth and selector deny decisions via deterministic pre-dispatch decision cache with ACL epoch invalidation.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-004/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-004/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-004/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-004/isomorphism_report.md`

## Replay commands

- Unit threat suite: `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_004`
- E2E threat suite: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004`
- Hardened replay: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004_HARDENED`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-004/env.json`
- `artifacts/phase2c/FR-P2C-004/manifest.json`
- `artifacts/phase2c/FR-P2C-004/repro.lock`
- `artifacts/phase2c/FR-P2C-004/LEGAL.md` (required when IP/provenance risk is plausible)

## Residual risks

- Current Rust runtime has no implemented ACL/auth subsystem; risk controls are contractual until implementation beads land.
- Selector reduction performance and cache invalidation correctness are high-risk until unit + e2e adversarial suites exist.
- ACL admin command surface parity can drift if response shape/ordering checks are not encoded in conformance fixtures.
