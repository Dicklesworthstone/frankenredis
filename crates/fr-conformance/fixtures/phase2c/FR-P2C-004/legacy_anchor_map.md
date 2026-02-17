# FR-P2C-004 Legacy Anchor Map

Packet: `FR-P2C-004`  
Subsystem: ACL and auth policy  
Target crates: `crates/fr-command`, `crates/fr-runtime`, `crates/fr-config`, `crates/fr-conformance`  
Prepared by: `PeachKnoll`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact extracts line-anchored ACL/auth contracts from legacy Redis and
maps them to current FrankenRedis coverage, including normal/edge/adversarial
behavior rows and explicit failure reason codes for downstream verification.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-004-A01` | `legacy_redis_code/redis/src/acl.c:348-365` | Selector creation initializes command/key/channel permission sets with deterministic defaults. | Missing selector model in Rust ACL surface. |
| `FR-P2C-004-A02` | `legacy_redis_code/redis/src/acl.c:1037-1212` | `ACLSetSelector` enforces command/category/key/channel rule grammar and rejects malformed rules with deterministic errno classes. | Missing. |
| `FR-P2C-004-A03` | `legacy_redis_code/redis/src/acl.c:1284-1412` | `ACLSetUser` mutates user state (`on/off`, password sets, selectors, reset flows) with strict error semantics. | Missing. |
| `FR-P2C-004-A04` | `legacy_redis_code/redis/src/acl.c:1415-1423` | Default user bootstrap is permissive (`+@all`, `~*`, `&*`, `on`, `nopass`) until overridden by config/ACL file. | Missing explicit default-user policy in Rust. |
| `FR-P2C-004-A05` | `legacy_redis_code/redis/src/acl.c:1426-1433` | ACL subsystem initialization is required before networking/client auth state setup. | Missing dedicated ACL init stage. |
| `FR-P2C-004-A06` | `legacy_redis_code/redis/src/acl.c:1441-1474` | Credential checks are fail-closed for unknown/disabled users and constant-time password compare on hashes. | Missing auth credential pipeline. |
| `FR-P2C-004-A07` | `legacy_redis_code/redis/src/acl.c:1512-1519` | Auth flow is module-first then password fallback, returning `AUTH_OK`/`AUTH_ERR`/`AUTH_BLOCKED`. | Missing. |
| `FR-P2C-004-A08` | `legacy_redis_code/redis/src/acl.c:1694-1762` | Per-selector permission checks gate command bits, key access patterns, and channel access patterns. | Missing selector-level evaluator. |
| `FR-P2C-004-A09` | `legacy_redis_code/redis/src/acl.c:1853-1904` | Multi-selector permission reduction chooses deterministic denial reason and argv index for diagnostics/logging. | Missing command-permission reducer. |
| `FR-P2C-004-A10` | `legacy_redis_code/redis/src/acl.c:1907-1909` | `ACLCheckAllPerm` is the high-level per-client command gate. | Missing runtime ACL gate. |
| `FR-P2C-004-A11` | `legacy_redis_code/redis/src/acl.c:2184-2230` | Pending ACL user definitions are prevalidated in fake users before activation. | Missing staged ACL-load validator. |
| `FR-P2C-004-A12` | `legacy_redis_code/redis/src/acl.c:2235-2277` | Config-defined users load with duplicate/default-user reconciliation and strict rule parse failures. | Missing. |
| `FR-P2C-004-A13` | `legacy_redis_code/redis/src/acl.c:2302-2492` | ACL file load is transactional: any syntax/semantic failure yields rollback and no partial activation. | Missing transactional ACL-file loader. |
| `FR-P2C-004-A14` | `legacy_redis_code/redis/src/acl.c:2499-2574` | ACL save is atomic temp-write + fsync + rename + dir fsync. | Missing ACL persistence path. |
| `FR-P2C-004-A15` | `legacy_redis_code/redis/src/acl.c:2581-2607` | Startup fails closed on mixed ACL sources (`aclfile` + configured users) or ACL load errors. | Missing startup ACL source policy. |
| `FR-P2C-004-A16` | `legacy_redis_code/redis/src/acl.c:2693-2788` | ACL denial log records grouped entries with bounded size and reason/object/context metadata. | Missing ACL denial structured log surface. |
| `FR-P2C-004-A17` | `legacy_redis_code/redis/src/acl.c:2875-3203` | `ACL` command family (`SETUSER`, `GETUSER`, `LOG`, `LOAD`, `SAVE`, etc.) defines user-visible admin semantics. | Missing ACL command namespace in router. |
| `FR-P2C-004-A18` | `legacy_redis_code/redis/src/acl.c:3254-3300` | `AUTH` command supports 1-arg default-user and 2-arg user/password forms with redaction and error policy. | Missing `AUTH` command in router/runtime. |
| `FR-P2C-004-A19` | `legacy_redis_code/redis/src/acl.c:3304-3313` | `requirepass` compatibility bridge mutates default user password/nopass state. | Missing requirepass bridge. |
| `FR-P2C-004-A20` | `legacy_redis_code/redis/src/networking.c:110-119` | `authRequired` depends on default-user flags and per-client authenticated state. | Missing per-client auth state. |
| `FR-P2C-004-A21` | `legacy_redis_code/redis/src/networking.c:3504-3511` | Unauthenticated clients use lookahead=1 to avoid post-AUTH parsing hazards and memory amplification. | Missing unauth lookahead policy. |
| `FR-P2C-004-A22` | `legacy_redis_code/redis/src/networking.c:4717-4780` | `HELLO ... AUTH` performs in-band auth with strict failure semantics; unauthenticated HELLO fails with `-NOAUTH`. | Missing HELLO/AUTH coupling. |
| `FR-P2C-004-A23` | `legacy_redis_code/redis/src/server.c:4403-4427` | Command execution enforces noauth gate (`CMD_NO_AUTH` exceptions) then ACL permission gate (`-NOPERM` + log). | Missing command pre-dispatch auth/ACL gate. |
| `FR-P2C-004-A24` | `legacy_redis_code/redis/src/server.c:3092`, `legacy_redis_code/redis/src/server.c:7758-7759`, `legacy_redis_code/redis/src/server.c:7969` | Server init ordering requires early ACL init, requirepass bridge apply, and startup user load before listener start. | Missing ACL-aware init ordering. |
| `FR-P2C-004-A25` | `legacy_redis_code/redis/src/config.c:2617-2624` | Config `requirepass` updates are translated into default-user ACL mutation. | Missing config-to-auth wiring. |
| `FR-P2C-004-A26` | `legacy_redis_code/redis/redis.conf:1079-1086` | `requirepass` is compatibility mode over ACL default user and conflicts with `aclfile`/`ACL LOAD`. | Missing config compatibility contract text in Rust docs/config. |
| `FR-P2C-004-A27` | `crates/fr-command/src/lib.rs:47-137`, `crates/fr-runtime/src/lib.rs:98-227`, `crates/fr-config/src/lib.rs:71-117` | Current Rust baseline dispatches data/TTL commands and compatibility gates, but has no AUTH/ACL/requirepass policy path. | Present as non-ACL substrate only. |

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-004-B01` | Normal | New client with default user in `nopass` mode | Client starts authenticated without explicit `AUTH`; no `-NOAUTH` for regular commands. | `FR-P2C-004-U001` | `FR-P2C-004-E001` | `auth.default_nopass_bootstrap_mismatch` |
| `FR-P2C-004-B02` | Normal | `AUTH <password>` against default user with configured password | Success returns `+OK` and flips authenticated state deterministically. | `FR-P2C-004-U002` | `FR-P2C-004-E002` | `auth.auth_command_success_state_mismatch` |
| `FR-P2C-004-B03` | Normal | `AUTH` wrong password / disabled user | `-WRONGPASS...` response and denial log entry are emitted; client remains unauthenticated. | `FR-P2C-004-U003` | `FR-P2C-004-E003` | `auth.auth_command_wrongpass_response_mismatch` |
| `FR-P2C-004-B04` | Normal | `HELLO 3 AUTH <user> <pass>` | On success, auth and protocol selection complete in one flow; on failure, early return with auth error. | `FR-P2C-004-U004` | `FR-P2C-004-E004` | `auth.hello_auth_flow_semantics_mismatch` |
| `FR-P2C-004-B05` | Edge | Unauthenticated client executes non-`CMD_NO_AUTH` command | Server returns `-NOAUTH Authentication required.` before normal command execution path. | `FR-P2C-004-U005` | `FR-P2C-004-E005` | `auth.noauth_gate_violation` |
| `FR-P2C-004-B06` | Edge | Authenticated client hits ACL denial for command/key/channel | Server returns `-NOPERM ...`, logs denial, and references deterministic denied argv position. | `FR-P2C-004-U006` | `FR-P2C-004-E006` | `auth.command_perm_resolution_mismatch` |
| `FR-P2C-004-B07` | Edge | `ACL SETUSER` with malformed rule/category/firstarg usage | Rule update fails with deterministic syntax/category semantics; partial mutation is rejected. | `FR-P2C-004-U007` | `FR-P2C-004-E007` | `auth.acl_selector_parse_validation_mismatch` |
| `FR-P2C-004-B08` | Edge | ACL file has one invalid user line among valid lines | Entire load rolls back; previously active ACL rules remain unchanged. | `FR-P2C-004-U008` | `FR-P2C-004-E008` | `auth.acl_file_transactional_load_violation` |
| `FR-P2C-004-B09` | Adversarial | Startup configured with both `aclfile` and inline ACL users | Startup hard-fails (fail closed), never enters service with ambiguous auth policy source. | `FR-P2C-004-U009` | `FR-P2C-004-E009` | `auth.acl_startup_source_conflict_not_failclosed` |
| `FR-P2C-004-B10` | Adversarial | Runtime `requirepass` config update path | Default-user credential mutation matches ACL bridge semantics (`resetpass` then set/nopass). | `FR-P2C-004-U010` | `FR-P2C-004-E010` | `auth.requirepass_bridge_drift` |
| `FR-P2C-004-B11` | Adversarial | Repeated ACL denials under burst traffic | ACL log groups equivalent entries, preserves unique IDs/timestamps, and respects max length bounds. | `FR-P2C-004-U011` | `FR-P2C-004-E011` | `auth.acl_log_grouping_contract_violation` |
| `FR-P2C-004-B12` | Adversarial | Large unauthenticated request stream before auth | Unauthenticated lookahead policy prevents multi-command speculative parse and rejects unsafe path deterministically. | `FR-P2C-004-U012` | `FR-P2C-004-E012` | `auth.unauthed_lookahead_policy_violation` |

## High-risk traceability and structured-log contract

For all `FR-P2C-004-U*` and `FR-P2C-004-E*` rows, logs must include:

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

Suggested suite IDs:

- Unit/property: `fr_auth_phase2c_packet_004`
- E2E/integration: `fr_runtime_phase2c_packet_004`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-004-CLAIM-01` |
| `evidence_id` | `FR-P2C-004-EVID-LEGACY-ACL-001` |
| Hotspot evidence | `A08`, `A09`, `A23` (selector checks + command-path gate dominate auth policy risk surface) |
| Mapped graveyard section IDs | `AG-SEC-11` (fail-closed admission control), `AG-DET-04` (deterministic reducer), `AG-AUD-03` (forensic logging contract) |
| Baseline comparator | Legacy Redis ACL/auth path (`acl.c` + `server.c` + `networking.c`) |
| EV score | `2.8` |
| Priority tier | `S` |
| Adoption wedge | Implement command-path auth gate first (`A23`) with deterministic reason codes, then selector/rule model (`A02`, `A08`, `A09`) |
| Budgeted mode defaults | Strict: `FailClosed`; Hardened: `BoundedDefense` only for allowlisted diagnostics |
| Deterministic exhaustion behavior | If hardened budget exhausted, force strict-equivalent fail-closed and emit `auth.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_004`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004` |

## Expected-loss decision model

States:

- `S0`: valid auth + ACL-allowed command
- `S1`: unauthenticated command attempt
- `S2`: authenticated but ACL-denied command/key/channel
- `S3`: config/ACL source ambiguity or corrupted ACL load input

Actions:

- `A0`: allow
- `A1`: reject with deterministic user-visible error (`-NOAUTH`/`-NOPERM`/`-WRONGPASS`)
- `A2`: bounded hardened defense with evidence emission
- `A3`: fail-closed startup/runtime halt

Loss matrix (lower is better):

| State \ Action | `A0` | `A1` | `A2` | `A3` |
|---|---:|---:|---:|---:|
| `S0` | 0.0 | 4.0 | 2.0 | 7.0 |
| `S1` | 10.0 | 0.0 | 2.0 | 6.0 |
| `S2` | 9.0 | 0.0 | 1.5 | 5.0 |
| `S3` | 12.0 | 4.0 | 3.0 | 0.0 |

Posterior/evidence terms:

- `P(S1|e)` rises with unauthenticated request rate and missing auth state transitions.
- `P(S2|e)` rises with selector mismatch events and ACL log denials.
- `P(S3|e)` rises with ACL source conflicts and transactional load parse errors.

Calibration and fallback policy:

- Calibration metric: Brier score target `<= 0.12` on deny/allow outcome forecasts.
- Fallback trigger: two consecutive windows with calibration breach or `S3` posterior `>= 0.20`.
- On trigger: disable hardened deviations for packet scope and force strict fail-closed until recalibrated.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever (downstream execution target):

- `LEV-004-H1`: replace repeated string-based runtime special-command routing checks (`AUTH`, `HELLO`, `ASKING`, `READONLY`, `READWRITE`, `CLUSTER`) with a length-bucketed byte classifier in `crates/fr-runtime/src/lib.rs`.

Required loop artifacts and paths:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-004/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-004/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-004/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-004/isomorphism_report.md`

Replay commands (strict/hardened):

- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004_STRICT`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004_HARDENED`

## Reproducibility and provenance pack references

Required pack (to be produced and linked by downstream packet beads):

- `artifacts/phase2c/FR-P2C-004/env.json`
- `artifacts/phase2c/FR-P2C-004/manifest.json`
- `artifacts/phase2c/FR-P2C-004/repro.lock`
- `artifacts/phase2c/FR-P2C-004/LEGAL.md` (mandatory if IP/provenance risk is detected)

## Confidence notes

- High confidence for command-path auth/ACL semantics and startup ordering (`acl.c`, `server.c`, `networking.c`, `config.c` anchors extracted directly).
- Medium confidence for optimized Rust implementation shape and cache invalidation details (pending downstream implementation beads).
