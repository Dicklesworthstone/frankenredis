# FR-P2C-003 Legacy Anchor Map

Packet: `FR-P2C-003`  
Subsystem: Command dispatch core  
Target crates: `crates/fr-command`, `crates/fr-runtime`, `crates/fr-conformance`  
Prepared by: `LavenderLake`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact extracts line-anchored command dispatch semantics from legacy
Redis request ingestion, command lookup, pre-dispatch policy checks, and command
execution plumbing. It then maps those contracts to current FrankenRedis
coverage and captures normal/edge/adversarial behavior rows with deterministic
reason-code requirements for downstream packet work.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-003-A01` | `legacy_redis_code/redis/src/networking.c:2936-3042` | Inline protocol path splits one line into argv, tracks input bytes, and emits deterministic read errors for malformed quoting/oversize payloads. | Partial: `frame_to_argv` decodes RESP array frames only; no inline protocol surface in Rust runtime (`crates/fr-command/src/lib.rs:27-45`). |
| `FR-P2C-003-A02` | `legacy_redis_code/redis/src/networking.c:2990-3001` | Master links must not send inline command payloads (except heartbeat newline); violations are explicit protocol read errors. | Missing explicit master-inline reject path in Rust runtime request ingestion. |
| `FR-P2C-003-A03` | `legacy_redis_code/redis/src/networking.c:3092-3134` | Multibulk parser enforces `*<count>` framing, integer parse validity, and unauthenticated command-count constraints before argv materialization. | Partial: RESP parser and runtime parse errors exist, but command-count gating by auth state is not mirrored at parser layer. |
| `FR-P2C-003-A04` | `legacy_redis_code/redis/src/networking.c:3180-3210` | Per-argument `$<len>` decoding enforces deterministic length validation and unauthenticated bulk-length limits. | Partial: protocol length validation exists; auth-conditioned bulk limits are not represented in current Rust parser/runtime pipeline. |
| `FR-P2C-003-A05` | `legacy_redis_code/redis/src/networking.c:3504-3511` | `processInputBuffer` sets lookahead to 1 for unauthenticated clients to prevent post-auth speculative parsing hazards. | Missing parser lookahead state machine in runtime command path. |
| `FR-P2C-003-A06` | `legacy_redis_code/redis/src/networking.c:3544-3578` | Request type dispatch (`inline` vs `multibulk`) is deterministic and sticky for pending command parsing. | Missing explicit dual-protocol request-type state in Rust runtime; current path assumes RESP arrays. |
| `FR-P2C-003-A07` | `legacy_redis_code/redis/src/networking.c:3589-3608` | Parsed pending commands are preprocessed and transferred into legacy `argc/argv` execution fields with looked-up command metadata. | Partial: runtime frame->argv conversion exists; no pending-command queue abstraction. |
| `FR-P2C-003-A08` | `legacy_redis_code/redis/src/networking.c:3690-3844` | Socket read path enforces query-buffer growth policy, big-arg handling, max query buffer limits, then calls command parser/executor. | Partial: `execute_bytes` parses one frame and executes immediately; no socket-level query buffer policy or per-client incremental parser state (`crates/fr-runtime/src/lib.rs:374-390`). |
| `FR-P2C-003-A09` | `legacy_redis_code/redis/src/server.c:3331-3368` | Command metadata population assigns ACL categories/IDs, key specs, and recursive subcommand relationships. | Missing generated command metadata registry in Rust command crate. |
| `FR-P2C-003-A10` | `legacy_redis_code/redis/src/server.c:3372-3394` | Command table is generated from static auto-generated table, with `commands` and `orig_commands` dictionaries for rename compatibility. | Missing registry generation and original-name fallback surface; Rust uses manual if-chain dispatch (`crates/fr-command/src/lib.rs:47-151`). |
| `FR-P2C-003-A11` | `legacy_redis_code/redis/src/server.c:3471-3496` | Lookup logic handles top-level vs one-level subcommands, with strict lookup mode for metadata queries. | Partial: runtime implements bespoke `CLUSTER HELP` subcommand path only (`crates/fr-runtime/src/lib.rs:499-528`). |
| `FR-P2C-003-A12` | `legacy_redis_code/redis/src/server.c:3551-3555` | `lookupCommandOrOriginal` preserves behavior when commands are renamed in runtime config. | Missing rename-command compatibility surface in Rust command routing. |
| `FR-P2C-003-A13` | `legacy_redis_code/redis/src/server.c:4178-4213` | Unknown-command handling distinguishes missing subcommand vs unknown command and caps argument preview length with newline sanitization. | Partial: unknown-command and args-preview text supported in command/runtime error mapping (`crates/fr-command/src/lib.rs:147-151`, `crates/fr-runtime/src/lib.rs:723-733`). |
| `FR-P2C-003-A14` | `legacy_redis_code/redis/src/server.c:4218-4227` | Arity validation enforces exact arity rules with canonical `"wrong number of arguments"` error family. | Present for implemented command set (`crates/fr-command/src/lib.rs:154-560`, `crates/fr-runtime/src/lib.rs:734-737`). |
| `FR-P2C-003-A15` | `legacy_redis_code/redis/src/server.c:4311-4315` | Command filters and request logging hooks run before normal lookup unless command is being reprocessed. | Missing command filter hook surface in Rust runtime. |
| `FR-P2C-003-A16` | `legacy_redis_code/redis/src/server.c:4330-4366` | Pre-dispatch lookup path reuses previous command when safe; then enforces command existence and arity before execution. | Partial: runtime dispatches via `dispatch_argv` with command-error conversion but has no reusable lookup cache path. |
| `FR-P2C-003-A17` | `legacy_redis_code/redis/src/server.c:4403-4427` | Admission order is explicit: noauth gate precedes ACL permission checks; denials emit deterministic error families (`NOAUTH`, `NOPERM`). | Partial: runtime implements noauth pre-dispatch gate with threat event (`crates/fr-runtime/src/lib.rs:331-349`); ACL reducer still missing. |
| `FR-P2C-003-A18` | `legacy_redis_code/redis/src/server.c:4433-4451` | Cluster redirection occurs in dispatch pipeline based on key-slot ownership and command key metadata. | Partial scaffold only: `CLUSTER HELP` and mode flags without key-slot redirect semantics (`crates/fr-runtime/src/lib.rs:499-528`). |
| `FR-P2C-003-A19` | `legacy_redis_code/redis/src/server.c:4660-4675` | Transaction-aware dispatch queues commands under MULTI and executes via `call()` otherwise. | Missing MULTI/EXEC queue semantics in current Rust runtime command path. |
| `FR-P2C-003-A20` | `legacy_redis_code/redis/src/server.c:3830-3889` | `call()` snapshots timing context, updates cached time with monotonic clock policy, and enters execution unit boundaries deterministically. | Missing equivalent call-layer timing snapshot API in runtime dispatch surface. |
| `FR-P2C-003-A21` | `legacy_redis_code/redis/src/server.c:3937-3973` | Execution updates latency/commandstats/slowlog/monitor hooks with blocked-client handling rules. | Missing commandstats/slowlog/monitor parity surface in Rust runtime. |
| `FR-P2C-003-A22` | `legacy_redis_code/redis/src/server.c:3999-4035` | Propagation to AOF/replication is driven by dirty state plus per-command override flags with fail-safe gating. | Missing call-layer propagation policy in runtime dispatch path. |
| `FR-P2C-003-A23` | `legacy_redis_code/redis/src/commands.c:4-13`, `legacy_redis_code/redis/src/commands.def:1-2`, `legacy_redis_code/redis/src/commands.def:1108-1138` | Command registry is generated code from command specs and is the canonical source for dispatch metadata, groups, and subcommand tables. | Missing generated command metadata system in Rust command crate. |
| `FR-P2C-003-A24` | `crates/fr-command/src/lib.rs:47-151` | Current Rust dispatch is explicit ASCII-insensitive branch chain for implemented command subset with deterministic unknown-command error object. | Present baseline, but does not model generated metadata, subcommand hierarchy, or policy flags. |
| `FR-P2C-003-A25` | `crates/fr-runtime/src/lib.rs:323-371`, `crates/fr-runtime/src/lib.rs:986-995` | Runtime pre-dispatch command router handles AUTH/HELLO/cluster-mode commands and enforces noauth before forwarding into `dispatch_argv`. | Present for current scope; ACL/cluster redirection/transaction queue contracts remain open. |
| `FR-P2C-003-A26` | `crates/fr-conformance/fixtures/core_errors.json:5-27`, `crates/fr-conformance/fixtures/core_strings.json:5-20`, `crates/fr-command/src/lib.rs:730-744` | Existing conformance + unit fixtures assert unknown-command, args-preview, wrong-arity, and base command happy paths. | Present but packet-specific FR-P2C-003 fixture family is still missing. |

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-003-B01` | Normal | RESP multibulk command (`*<n>`) with valid lengths | Command is parsed and dispatched with deterministic argv order and command identity. | `FR-P2C-003-U001` | `FR-P2C-003-E001` | `dispatch.multibulk_parse_dispatch_mismatch` |
| `FR-P2C-003-B02` | Normal | Inline command line on non-master client | Inline command tokenization yields same argv semantics as equivalent multibulk form. | `FR-P2C-003-U002` | `FR-P2C-003-E002` | `dispatch.inline_parse_semantics_mismatch` |
| `FR-P2C-003-B03` | Normal | Known command with valid arity | Command lookup resolves deterministically and returns expected reply family. | `FR-P2C-003-U003` | `FR-P2C-003-E003` | `dispatch.lookup_known_command_mismatch` |
| `FR-P2C-003-B04` | Normal | Command with one-level subcommand (`CONTAINER SUBCMD`) | Lookup chooses subcommand path when present and strict arity rules are respected. | `FR-P2C-003-U004` | `FR-P2C-003-E004` | `dispatch.subcommand_resolution_mismatch` |
| `FR-P2C-003-B05` | Edge | Unknown command name | Error text class and args-preview behavior remain deterministic and newline-safe. | `FR-P2C-003-U005` | `FR-P2C-003-E005` | `dispatch.unknown_command_error_mismatch` |
| `FR-P2C-003-B06` | Edge | Missing/unknown subcommand for a container command | Error family distinguishes missing subcommand from unknown command and suggests help path. | `FR-P2C-003-U006` | `FR-P2C-003-E006` | `dispatch.unknown_subcommand_error_mismatch` |
| `FR-P2C-003-B07` | Edge | Wrong arity for known command | Canonical wrong-arity error is emitted before command side effects. | `FR-P2C-003-U007` | `FR-P2C-003-E007` | `dispatch.wrong_arity_error_mismatch` |
| `FR-P2C-003-B08` | Edge | Unauthenticated command when auth is required | Admission gate rejects with `NOAUTH` before command execution path. | `FR-P2C-003-U008` | `FR-P2C-003-E008` | `dispatch.noauth_gate_order_violation` |
| `FR-P2C-003-B09` | Edge | Authenticated command denied by ACL reducer | Dispatch returns `NOPERM` with deterministic denial diagnostics and no side effects. | `FR-P2C-003-U009` | `FR-P2C-003-E009` | `dispatch.acl_denial_contract_violation` |
| `FR-P2C-003-B10` | Edge | MULTI context with non-transaction-control command | Command is queued (not immediately executed) with deterministic queued reply semantics. | `FR-P2C-003-U010` | `FR-P2C-003-E010` | `dispatch.multi_queue_semantics_mismatch` |
| `FR-P2C-003-B11` | Adversarial | Request targeting `host:`/`post` fake command probe | Security warning path triggers and connection is terminated according to policy. | `FR-P2C-003-U011` | `FR-P2C-003-E011` | `dispatch.security_probe_not_rejected` |
| `FR-P2C-003-B12` | Adversarial | Malformed multibulk length / malformed bulk length | Parsing fails deterministically without partial command execution. | `FR-P2C-003-U012` | `FR-P2C-003-E012` | `dispatch.malformed_length_acceptance` |
| `FR-P2C-003-B13` | Adversarial | Master sends non-empty inline command payload | Input path rejects master inline protocol command and records protocol read error. | `FR-P2C-003-U013` | `FR-P2C-003-E013` | `dispatch.master_inline_protocol_violation` |
| `FR-P2C-003-B14` | Adversarial | Oversized unauthenticated request stream | Unauthenticated lookahead/query-buffer safety gates block amplification and preserve deterministic errors. | `FR-P2C-003-U014` | `FR-P2C-003-E014` | `dispatch.unauth_buffer_guard_violation` |
| `FR-P2C-003-B15` | Adversarial | Command rename in runtime config + command rewrite path | Lookup fallback to original command table preserves execution mapping. | `FR-P2C-003-U015` | `FR-P2C-003-E015` | `dispatch.rename_lookup_fallback_mismatch` |
| `FR-P2C-003-B16` | Adversarial | Cluster key-slot mismatch on command path | Dispatch emits deterministic redirection behavior and avoids local execution side effects. | `FR-P2C-003-U016` | `FR-P2C-003-E016` | `dispatch.cluster_redirection_contract_violation` |
| `FR-P2C-003-B17` | Adversarial | Dirty write command under propagation-enabled path | AOF/replication propagation decisions honor dirty bit and explicit force/prevent flags. | `FR-P2C-003-U017` | `FR-P2C-003-E017` | `dispatch.propagation_decision_mismatch` |

## High-risk traceability and structured-log contract

For all `FR-P2C-003-U*` and `FR-P2C-003-E*` rows, logs must include:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-003`)
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

- Unit/property: `fr_dispatch_phase2c_packet_003`
- E2E/integration: `fr_runtime_phase2c_packet_003`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-003-CLAIM-01` |
| `evidence_id` | `FR-P2C-003-EVID-DISPATCH-001` |
| Hotspot evidence | `A05`, `A17`, `A22` (unauth lookahead + admission order + propagation path) |
| Mapped graveyard section IDs | `AG-SEC-11` (fail-closed admission), `AG-DET-04` (deterministic reducer), `AG-PIPE-03` (ingest-to-exec boundary integrity) |
| Baseline comparator | Legacy Redis dispatch chain (`networking.c` -> `processCommand` -> `call`) |
| EV score | `2.7` |
| Priority tier | `S` |
| Adoption wedge | Land packet-specific dispatch fixtures + reason-code matrix before extending command surface breadth |
| Budgeted mode defaults | Strict: `FailClosed`; Hardened: `BoundedDefense` only for explicit allowlisted diagnostics |
| Deterministic exhaustion behavior | Hardened budget exhaustion forces strict-equivalent reject path with `dispatch.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_003`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_003` |

## Expected-loss decision model

States:

- `S0`: valid parsed command and policy-allowed dispatch
- `S1`: malformed/incomplete command frame or parser desync risk
- `S2`: unauthorized or ACL-denied command path
- `S3`: propagation/redirection critical-path mismatch

Actions:

- `A0`: execute command
- `A1`: deterministic reject with canonical error family
- `A2`: bounded hardened defense + evidence emission
- `A3`: fail-closed block (startup/runtime)

Loss matrix (lower is better):

| State \\ Action | `A0` | `A1` | `A2` | `A3` |
|---|---:|---:|---:|---:|
| `S0` | 0.0 | 5.0 | 2.5 | 8.0 |
| `S1` | 11.0 | 0.0 | 2.0 | 5.0 |
| `S2` | 12.0 | 0.0 | 2.0 | 5.0 |
| `S3` | 10.0 | 4.0 | 3.0 | 1.0 |

Decision policy:

- if posterior(`S3`) >= `0.20`, enforce `A3` until parity evidence is restored;
- else if posterior(`S1`) + posterior(`S2`) >= `0.35`, use `A1`;
- else choose `A0`.

Calibration + fallback:

- calibration target: Brier score `<= 0.12` on allow/reject forecasts for dispatch rows;
- fallback trigger: two consecutive calibration breaches or any critical-row drift in `B08`, `B12`, `B17`;
- fallback action: disable non-allowlisted hardened deviations and force strict fail-closed behavior.

## One-lever extreme-optimization loop artifacts

Selected single lever (downstream execution target):

- `LEV-003-01`: command lookup acceleration cache keyed by `(command_name, argc_shape, mode)` with explicit invalidation on command registry mutation.

Required loop artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-003/baseline_profile.json`
- Lever selection note: `artifacts/phase2c/FR-P2C-003/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-003/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-003/isomorphism_report.md`

Replay commands:

- `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_003`
- `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_003`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_003`

## Reproducibility and provenance pack references

Required pack (to be produced by downstream packet beads):

- `artifacts/phase2c/FR-P2C-003/env.json`
- `artifacts/phase2c/FR-P2C-003/manifest.json`
- `artifacts/phase2c/FR-P2C-003/repro.lock`
- `artifacts/phase2c/FR-P2C-003/LEGAL.md` (mandatory if IP/provenance risk is detected)

## Sequencing boundary notes

- This bead covers extraction and behavior mapping only.
- `bd-2wb.14.2` should convert this artifact into strict/hardened contract-table rows.
- `bd-2wb.14.3+` should wire threat model, implementation plan, and fixture/report artifacts.

## Confidence notes

- High confidence on ingestion/lookup/admission-order anchors extracted directly from `networking.c` and `server.c`.
- High confidence on current Rust dispatch/noauth/error coverage anchors in `fr-command` and `fr-runtime`.
- Medium confidence on propagation/rename/subcommand parity gap closure estimates until packet-specific fixture families land.
