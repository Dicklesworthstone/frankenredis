# FR-P2C-009 Contract Table

Packet: `FR-P2C-009`  
Subsystem: TLS/config boundary  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-009/legacy_anchor_map.md`

## Contract row schema (normative)

Each row defines:

- `trigger`: deterministic TLS/config event.
- `preconditions`: required state before contract evaluation.
- `strict_contract`: Redis-observable behavior that must match legacy semantics.
- `hardened_contract`: bounded defensive behavior preserving API/ordering contract.
- `fail_closed_boundary`: mandatory hard-failure edge.
- `unit_trace` / `e2e_trace`: required verification mapping.
- `reason_codes`: deterministic diagnostics required on mismatch.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-009-C01` | `tls-protocols` parsing | TLS protocols string provided | Supported protocol tokens are parsed deterministically and unsupported tokens are rejected/normalized per legacy contract. | Same with richer diagnostics only. | Invalid protocol token accepted with behavior-changing fallback. | `FR-P2C-009-U001` | `FR-P2C-009-E001` | `tlscfg.protocols_parse_contract_violation` |
| `FR-P2C-009-C02` | TLS context construction | TLS cert/key/CA/cipher fields present | Context build enforces cert/key/CA/cipher invariants and fails atomically on invalid material. | Same. | Partially initialized TLS context becomes observable. | `FR-P2C-009-U002` | `FR-P2C-009-E002` | `tlscfg.context_build_contract_violation` |
| `FR-P2C-009-C03` | `tlsConfigure`-equivalent runtime apply | Existing and candidate TLS context available | Runtime TLS config apply swaps active contexts only after complete successful validation/setup. | Same. | Context pointers change after partial/failed apply. | `FR-P2C-009-U003` | `FR-P2C-009-E003` | `tlscfg.atomic_reconfigure_violation` |
| `FR-P2C-009-C04` | Listener bootstrap/update | `tls_port` or TLS policy flags known | TLS listeners/register hooks are enabled only when TLS policy requires them and remain consistent with listener state. | Same. | TLS listener state diverges from config policy. | `FR-P2C-009-U004` | `FR-P2C-009-E004` | `tls.listener_bootstrap_contract_violation` |
| `FR-P2C-009-C05` | Accepted TLS connection policy application | Accepted socket + `tls-auth-clients` mode | Peer verification mode maps deterministically to policy (`off`, optional, required). | Same. | Accepted connection bypasses required verify policy. | `FR-P2C-009-U005` | `FR-P2C-009-E005` | `tls.handshake_verify_policy_violation` |
| `FR-P2C-009-C06` | TLS handshake/I/O state transition | Non-blocking SSL operation active | WANT_READ/WANT_WRITE/error/closed transitions and event re-arming remain deterministic. | Same with bounded diagnostics. | Handshake/I/O state machine enters inconsistent or blocking state. | `FR-P2C-009-U005` | `FR-P2C-009-E005` | `tls.io_state_transition_violation` |
| `FR-P2C-009-C07` | TLS read/write path execution | Established TLS session + I/O budget | Read/write behavior preserves bounded-per-event writes and deterministic EAGAIN/errno semantics. | Same. | I/O budget or errno semantics drift from contract. | `FR-P2C-009-U006` | `FR-P2C-009-E006` | `tls.io_budget_errno_contract_violation` |
| `FR-P2C-009-C08` | Peer identity extraction | Cert-auth enabled with configured identity field | Configured certificate field extraction is deterministic and policy-compliant for auth mapping. | Same. | Wrong/missing cert field accepted as identity. | `FR-P2C-009-U007` | `FR-P2C-009-E007` | `tls.peer_identity_contract_violation` |
| `FR-P2C-009-C09` | TLS directive registration/validation | Config table initialization and mutation path active | TLS directives (`tls-port`, cert/key/CA, protocols/ciphers, auth/session settings) preserve declared mutability/sensitivity and validation contracts. | Same. | Config directives mutate outside declared policy envelope. | `FR-P2C-009-U008` | `FR-P2C-009-E008` | `tlscfg.directive_registry_contract_violation` |
| `FR-P2C-009-C10` | `bind` apply with TCP/TLS listeners | Address bind mutation requested | Paired TCP/TLS listener transitions are atomic and rollback-safe on TLS bind failures. | Same. | Mixed listener state persists after failed bind apply. | `FR-P2C-009-U009` | `FR-P2C-009-E009` | `tlscfg.bind_atomicity_violation` |
| `FR-P2C-009-C11` | Runtime TLS config mutation (`CONFIG SET`) | TLS config values changed at runtime | TLS context and listeners are reconfigured deterministically without restart and without partial state leaks. | Same with bounded diagnostics only. | Runtime TLS mutation leaves stale context/listener state. | `FR-P2C-009-U010` | `FR-P2C-009-E010` | `tlscfg.runtime_apply_contract_violation` |
| `FR-P2C-009-C12` | Connection-type configure hook call | TLS connection type registered | `connTypeConfigure`-equivalent hook is invoked deterministically for startup and runtime apply paths. | Same. | TLS context updates bypass connection-type configure path. | `FR-P2C-009-U010` | `FR-P2C-009-E010` | `tlscfg.connection_type_configure_violation` |
| `FR-P2C-009-C13` | TLS-specific operational knobs evaluation | Cluster announce and accept-limit fields configured | `cluster-announce-tls-port` and `max-new-tls-connections-per-cycle` semantics are enforced deterministically. | Same. | TLS announce or accept-rate policies drift from config values. | `FR-P2C-009-U011` | `FR-P2C-009-E011` | `tlscfg.operational_knob_contract_violation`, `tls.accept_rate_limit_violation` |
| `FR-P2C-009-C14` | Config rewrite/persistence | Runtime config rewrite executed | Rewrite output preserves TLS directives and deterministic ordering for restart parity. | Same. | TLS directives are dropped/reordered incompatibly during rewrite. | `FR-P2C-009-U012` | `FR-P2C-009-E012` | `tlscfg.rewrite_persistence_violation` |
| `FR-P2C-009-C15` | TLS/config safety gate evaluation | Malformed or contradictory TLS/config state detected | Unsafe TLS/config transitions are blocked fail-closed before listener/context mutation. | Allowlisted bounded sanitization only when strict-equivalent. | Unsafe mutation proceeds after safety gate violation. | `FR-P2C-009-U013` | `FR-P2C-009-E013` | `tlscfg.safety_gate_contract_violation` |
| `FR-P2C-009-C16` | Hardened non-allowlisted deviation candidate | Mode=`hardened` and deviation unresolved | Strict-equivalent fail-closed baseline unless deviation category is explicitly allowlisted. | Only allowlisted bounded defenses may proceed with policy evidence. | Non-allowlisted deviation changes outward TLS/config semantics. | `FR-P2C-009-U013` | `FR-P2C-009-E013` | `tlscfg.hardened_nonallowlisted_rejected`, `tlscfg.hardened_policy_violation` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-009-I01` | TLS protocol parsing determinism | Required | Required |
| `FR-P2C-009-I02` | TLS context build atomicity | Required | Required |
| `FR-P2C-009-I03` | Runtime TLS reconfiguration atomicity | Required | Required |
| `FR-P2C-009-I04` | Listener-policy alignment | Required | Required |
| `FR-P2C-009-I05` | Handshake verify-policy enforcement | Required | Required |
| `FR-P2C-009-I06` | TLS I/O state-machine determinism | Required | Required |
| `FR-P2C-009-I07` | Config directive registry/validation consistency | Required | Required |
| `FR-P2C-009-I08` | Bind/apply rollback safety | Required | Required |
| `FR-P2C-009-I09` | Runtime reconfigure + connection-type hook consistency | Required | Required |
| `FR-P2C-009-I10` | TLS operational knob enforcement | Required | Required |
| `FR-P2C-009-I11` | Config rewrite persistence parity | Required | Required |
| `FR-P2C-009-I12` | Non-allowlisted hardened deviation handling | N/A (strict fail-closed baseline) | Reject non-allowlisted deviations |

## Allowed hardened deviations (bounded)

- `BoundedParserDiagnostics`: richer parse diagnostics for TLS/config parsing without behavior drift.
- `MetadataSanitization`: bounded config sanitation only when strict-equivalent.
- `ResourceClamp`: bounded resource controls for handshake/listener pressure without observable API drift.

Non-allowlisted behavior differences are rejected and treated as `fail_closed`.

## Structured-log contract for FR-P2C-009 rows

Each contract-row verification result (pass/fail and divergence checks) must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-009`)
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

- Strict unit replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-config -- --nocapture fr_p2c_009_u001_protocol_parse_rejects_unknown_token`
- Strict runtime replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u013_strict_mode_rejects_unsafe_tls_config_and_records_event`
- Hardened runtime replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u013_hardened_non_allowlisted_tls_deviation_is_rejected`
- Strict conformance replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_strict_runtime_tls_rejection_matches_expected_threat_contract`
- Hardened conformance replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-009-CLAIM-02` |
| `evidence_id` | `FR-P2C-009-EVID-CONTRACT-001` |
| Hotspot evidence | `C03`, `C06`, `C11` (atomic reconfigure, TLS I/O state machine, runtime apply path) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-SEC-11`, `AG-NET-06` |
| Baseline comparator | Legacy Redis TLS/config runtime contract |
| EV score | `3.1` |
| Priority tier | `S` |
| Adoption wedge | Land parse/build/apply atomicity first, then handshake/I/O semantics, then rewrite/operational knobs and hardened gates |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | On budget exhaustion force strict-equivalent fail-closed with `tlscfg.hardened_budget_exhausted_failclosed` |
| Replay commands | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-config -- --nocapture fr_p2c_009_u001_protocol_parse_rejects_unknown_token`; `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract` |

## Expected-loss decision model

States:

- `S0`: contract-preserving TLS/config behavior
- `S1`: bounded recoverable condition (allowlisted)
- `S2`: unsafe TLS/config divergence condition

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

- `P(S1|e)`: bounded parser/metadata anomalies without listener/context drift.
- `P(S2|e)`: TLS context apply failure, handshake-state drift, or rewrite/apply contract mismatch.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive calibration breaches or critical row drift (`C03`, `C10`, `C11`, `C16`).
- Fallback behavior: disable hardened deviations and enforce strict fail-closed packet mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-009-02`: deterministic TLS-config apply planner cache keyed by `(tls_cfg_digest, listener_mask, policy_epoch)` with strict invalidation on config mutation and listener rebind events.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-009/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-009/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-009/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-009/isomorphism_report.md`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-009/env.json`
- `artifacts/phase2c/FR-P2C-009/manifest.json`
- `artifacts/phase2c/FR-P2C-009/repro.lock`
- `artifacts/phase2c/FR-P2C-009/LEGAL.md` (required if IP/provenance risk is found)

## Traceability checklist

- Every contract row maps to at least one unit ID and one e2e ID.
- Every contract row declares deterministic `reason_code` values.
- Every contract row includes explicit strict/hardened expectations and fail-closed boundary.
- User-visible TLS/config outcomes are explicit for config apply, listener state, and handshake behavior.
