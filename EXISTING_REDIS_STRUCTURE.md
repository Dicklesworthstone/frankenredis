# EXISTING_REDIS_STRUCTURE

## 1. Legacy Oracle

- Root: `/data/projects/frankenredis/legacy_redis_code/redis`
- Upstream: `https://github.com/redis/redis`
- Role: behavioral oracle for reply bytes, side effects, ordering, and replay semantics.

## 2. Subsystem Map

| Subsystem | Legacy Paths | Parity Critical Notes |
|---|---|---|
| Event loop + networking | `src/ae.c`, `src/networking.c`, `src/connection.c` | request lifecycle ordering, short write behavior, backpressure behavior |
| Protocol parser | `src/networking.c`, RESP decode paths | frame boundary handling, malformed frame errors, inline/protocol mode edges |
| Command router | `src/server.c`, command table declarations | arity checks, flags, command deny/allow conditions |
| Data model | `src/db.c`, `src/object.c`, `src/dict.c`, `src/quicklist.c`, `src/listpack.c`, `src/rax.c` | deterministic mutation semantics and keyspace side effects |
| Expiry/eviction | `src/expire.c`, `src/evict.c` | TTL return codes (`-2`, `-1`, positive), active/passive expiry interactions |
| Persistence | `src/aof.c`, `src/rdb.c`, `src/rio.c`, `src/bio.c` | replay ordering, data-loss boundaries, corruption handling |
| Replication | `src/replication.c` | offsets, backlog semantics, PSYNC transitions, ordering on reconnect |
| Security/config | `src/acl.c`, `src/config.c`, `src/tls.c` | fail-closed behavior and configuration compatibility |

## 3. Semantics To Preserve Exactly (Full-Parity Program)

1. RESP parse validity boundaries and error class mapping.
2. Command arity and command error strings for the full command-family parity target.
3. Command side effects and reply shape parity.
4. TTL behavior including deletion on immediate expiry.
5. AOF/RDB replay ordering invariants.
6. Replication offset monotonicity and lag accounting semantics.

## 4. Extraction Completeness Matrix (Current)

| Domain | Status | Notes |
|---|---|---|
| Protocol framing | partial | first parser scaffold in Rust done; malformed corpus still incomplete |
| Command table semantics | partial | bootstrap set implemented (`PING/ECHO/SET/GET/DEL/INCR/EXPIRE/PTTL`) |
| Data structure parity | partial | string path only in runtime slice |
| TTL/expiry edge cases | partial | baseline codes and lazy expiry in place |
| Persistence replay | partial | AOF frame contract scaffolded, full replay not yet landed |
| Replication behavior | partial | state/offset scaffolding only |
| ACL/config semantics | not_started | reserved for M3+ |

## 5. Extraction Sequencing Boundary (No Permanent Exclusions)

Current sequencing tranche:
- protocol framing, command core, keyspace string semantics, TTL core, AOF/RDB baseline, replication baseline.

Deferred in sequence (still mandatory for final parity):
- sentinel, module API, full-cluster orchestration, full TLS/IO-thread parity, scripting/module surface.

Rule:
- deferred means "next in queue with blocking closure criteria", not "excluded from scope".

## 6. Conformance Fixture Families (Planned)

1. `protocol_core`: valid and malformed RESP frames.
2. `string_core`: deterministic string command semantics.
3. `ttl_core`: expiry edge cases and return-code parity.
4. `persist_core`: replay ordering and state recovery parity.
5. `repl_core`: offset progression and handshake parity.
6. `security_config_core`: fail-closed compatibility checks.

## 7. Drift Severity Taxonomy

- `critical`: reply bytes / side effects / ordering mismatch.
- `high`: compatibility behavior mismatch without immediate data corruption.
- `medium`: recoverable mismatch with explicit exception.
- `low`: non-contract observability differences only.

## 8. DOC-PASS-00 Baseline Gap Matrix + Quantitative Expansion Targets

### 8.1 Scoring rubric (auditable)

- Coverage score (`0..5`): `0` absent, `1` skeletal bullets, `2` partial map, `3` substantial map with missing edge handling, `4` near-complete with evidence anchors, `5` full operational contract.
- Evidence quality: `low` (claims without reproducible artifacts), `medium` (some measured data/paths), `high` (fixture IDs, commands, and artifact pointers).
- Expansion factor: minimum multiplier required for section rewrite depth in DOC-PASS-01..14.

### 8.2 Section-by-section matrix (this document)

| Section | Baseline lines | Coverage | Evidence | Primary omissions to close | Expansion factor | Target lines | Risk | Downstream bead |
|---|---:|---:|---|---|---:|---:|---|---|
| Title + framing (`# EXISTING_REDIS_STRUCTURE`) | 2 | 1 | low | Missing explicit statement of scope boundaries and update policy | 6x | 12 | medium | `bd-2wb.24.14` |
| 1. Legacy Oracle | 6 | 2 | low | Missing branch/commit pinning, oracle invocation contract, and replay entrypoints | 8x | 48 | high | `bd-2wb.24.2` |
| 2. Subsystem Map | 13 | 2 | medium | Missing ownership boundaries, coupling direction, and adapter boundaries | 14x | 182 | critical | `bd-2wb.24.2` |
| 3. Semantics To Preserve Exactly | 9 | 2 | low | Missing command-family granularity and strict/hardened divergence rules | 12x | 108 | critical | `bd-2wb.24.3` |
| 4. Extraction Completeness Matrix | 11 | 2 | low | Missing measurable closure criteria, fixture IDs, and reason-code traceability | 12x | 132 | critical | `bd-2wb.24.4` |
| 5. Extraction Sequencing Boundary | 11 | 2 | low | Missing dependency DAG, temporary deferment criteria, and unblock ordering | 14x | 154 | high | `bd-2wb.24.9` |
| 6. Conformance Fixture Families | 9 | 2 | low | Missing deterministic seeds, replay commands, and packet linkage | 14x | 126 | critical | `bd-2wb.24.10` |
| 7. Drift Severity Taxonomy | 6 | 2 | low | Missing severity thresholds, escalation routing, and operator runbooks | 12x | 72 | high | `bd-2wb.24.6` |

Current baseline for this file: `68` lines.  
Target range for final rewrite: `780-960` lines (about `11.5x-14.1x`).

### 8.3 Completion gates for DOC-PASS-00 coverage in this file

DOC-PASS-00 coverage for `EXISTING_REDIS_STRUCTURE.md` is complete when:
1. Every top-level section plus title framing has a baseline line count, coverage score, omissions list, and numeric target.
2. All omissions are tied to downstream beads and remain dependency-auditable.
3. Quantitative target range is explicit and reproducible from repository content.

## 9. DOC-PASS-01 Workspace Cartography (Current Codebase)

### 9.1 Crate dependency graph (conflict-checked against `crates/*/Cargo.toml`)

Dependency edges (`A -> B` means `A` depends on `B`):

1. `fr-command -> fr-protocol`
2. `fr-command -> fr-store`
3. `fr-persist -> fr-protocol`
4. `fr-runtime -> fr-command`
5. `fr-runtime -> fr-config`
6. `fr-runtime -> fr-eventloop`
7. `fr-runtime -> fr-protocol`
8. `fr-runtime -> fr-store`
9. `fr-conformance -> fr-config`
10. `fr-conformance -> fr-persist`
11. `fr-conformance -> fr-protocol`
12. `fr-conformance -> fr-repl`
13. `fr-conformance -> fr-runtime`
14. `fr-conformance -> serde`
15. `fr-conformance -> serde_json`

`fr-expire` is currently an isolated crate at the Cargo graph level (no inbound or outbound workspace edges).

### 9.2 Layered map (current implementation)

| Layer | Crates | Role |
|---|---|---|
| L0 primitives | `fr-protocol`, `fr-store`, `fr-expire`, `fr-repl`, `fr-eventloop`, `fr-config` | low-level contracts and deterministic helper/state logic |
| L1 operation kernels | `fr-command`, `fr-persist` | command execution kernel and persistence stream kernel |
| L2 runtime assembly | `fr-runtime` | admission gates, protocol-to-command execution path, evidence ledger, runtime policy application |
| L3 verification/oracle | `fr-conformance` | fixture harness, live-oracle differential execution, structured-log validation |

Allowed direction: `L3 -> L2 -> L1 -> L0` and `L3 -> L0` for direct verification helpers.  
Disallowed direction (design intent): lower layers importing higher layers.

## 10. Ownership Boundaries by Crate

| Crate | Primary file(s) now | Owned boundary | Key exported surface | Inbound callers |
|---|---|---|---|---|
| `fr-protocol` | `crates/fr-protocol/src/lib.rs` | RESP frame model + parse/encode fail-closed behavior | `RespFrame`, `RespParseError`, `parse_frame` | `fr-runtime`, `fr-command`, `fr-persist`, `fr-conformance` |
| `fr-store` | `crates/fr-store/src/lib.rs` | in-memory key/value state, mutation semantics, TTL state attached to entries | `Store`, `StoreError`, `PttlValue` | `fr-command`, `fr-runtime` |
| `fr-expire` | `crates/fr-expire/src/lib.rs` | reusable expiry decision helper logic | `ExpiryDecision`, `evaluate_expiry` | currently none |
| `fr-command` | `crates/fr-command/src/lib.rs` | argv normalization and command dispatch for string/ttl/keyspace command set | `frame_to_argv`, `dispatch_argv`, `CommandError` | `fr-runtime` |
| `fr-eventloop` | `crates/fr-eventloop/src/lib.rs` | deterministic tick planning, phase-order verification, bootstrap checks, TLS accept-rate planning | `plan_tick`, `run_tick`, `replay_phase_trace`, `validate_bootstrap` | `fr-runtime` |
| `fr-config` | `crates/fr-config/src/lib.rs` | strict/hardened policy model, TLS config validation/rewrite/runtime-plan contracts | `RuntimePolicy`, `TlsConfig`, `TlsCfgError`, `plan_tls_runtime_apply` | `fr-runtime`, `fr-conformance` |
| `fr-persist` | `crates/fr-persist/src/lib.rs` | AOF stream framing encode/decode contract | `AofRecord`, `encode_aof_stream`, `decode_aof_stream` | `fr-conformance` |
| `fr-repl` | `crates/fr-repl/src/lib.rs` | replication handshake/state/backlog/wait decision logic | `HandshakeFsm`, `ReplProgress`, `decide_psync`, `evaluate_wait*` | `fr-conformance` |
| `fr-runtime` | `crates/fr-runtime/src/lib.rs` | top-level execution assembly: parse/dispatch/auth/cluster gates/evidence/tls apply | `Runtime`, `EvidenceEvent`, runtime adapter traits | `fr-conformance` |
| `fr-conformance` | `crates/fr-conformance/src/lib.rs`, `src/log_contract.rs`, `src/phase2c_schema.rs` | fixture-driven conformance, live oracle diffing, structured-log contract validation | `run_fixture`, `run_protocol_fixture`, `run_replay_fixture`, `run_live_redis_diff` | test binaries and operators |

## 11. Request Lifecycle Cartography (Ownership by Stage)

| Stage | Input -> output | Owning crate | Concrete path |
|---|---|---|---|
| Wire decode | request bytes -> `RespFrame` or `RespParseError` | `fr-protocol` | `parse_frame` |
| Runtime preflight | frame -> gate decision (array/bulk bounds) | `fr-runtime` | `Runtime::preflight_gate` |
| Command argv projection | `RespFrame` -> `Vec<Vec<u8>>` | `fr-command` | `frame_to_argv` |
| Admission/auth/cluster gate | argv -> pass/reject/cluster state updates | `fr-runtime` | `handle_auth_command`, `handle_hello_command`, `handle_cluster_command`, `handle_readonly/readwrite/asking` |
| Command execution | argv + store -> reply frame | `fr-command` + `fr-store` | `dispatch_argv` -> store methods |
| Protocol error mapping | parse/command errors -> Redis-visible error replies | `fr-runtime` | `protocol_error_to_resp`, `command_error_to_resp` |
| Evidence capture | runtime event -> structured evidence row | `fr-runtime` | `record_threat_event` + `EvidenceLedger` |
| Conformance verification | fixture case -> pass/fail differential report | `fr-conformance` | `run_fixture`, `run_protocol_fixture`, `run_live_redis_diff` |

## 12. Adapter Seams and Injection Points

1. Runtime ecosystem adapters:
   `fr-runtime::ecosystem::{AsyncRuntimeAdapter, OperatorUiAdapter}` isolate integration with external runtimes/UIs.
2. Structured log conversion seam:
   `fr-conformance::log_contract::StructuredLogEvent::from_runtime_evidence` converts runtime evidence to canonical forensic schema.
3. Packet schema gate seam:
   `fr-conformance::phase2c_schema` validates packet artifact topology and schema contracts independently from runtime execution.
4. Persistence stream seam:
   `fr-persist::{encode_aof_stream, decode_aof_stream}` is the boundary between command-level replay representation and byte-level transport encoding.

## 13. Hidden/Implicit Coupling Register (Current Risks)

| ID | Coupling | Why risky | Suggested boundary fix |
|---|---|---|---|
| HC-001 | TTL logic exists in `fr-store` while `fr-expire` is isolated | expiration semantics can drift across crates without compiler-level coupling | make `fr-store` consume `fr-expire` decision helpers or formally collapse ownership into one crate |
| HC-002 | ACL/auth/HELLO/cluster admission logic is in `fr-runtime` while command dispatch is in `fr-command` | command-policy rules are split across layers; coverage can miss one side | define explicit admission contract in `fr-command` or add a dedicated admission crate |
| HC-003 | `fr-repl` contracts are validated from `fr-conformance` but not yet wired into `fr-runtime` execution | replication invariants can pass in isolated tests but diverge in runtime integration | introduce runtime-owned replication state integration path and e2e fixture coverage |
| HC-004 | `fr-persist` accepts multiple RESP scalar argument forms for AOF record decode | relaxed acceptance can mask malformed replay surface differences | document and gate accepted replay frame classes in conformance fixtures |
| HC-005 | Several crates are single-file modules (`src/lib.rs` only) | ownership boundaries are logical, not file-enforced; high edit-collision risk in multi-agent sessions | split by contract domain (`parser`, `errors`, `state`, `validation`, `io`) with explicit mod boundaries |

## 14. Conflict Check Notes (Docs vs Code)

1. `fr-runtime` currently depends on `fr-eventloop` in Cargo, so architecture docs must include this edge explicitly.
2. `fr-conformance` currently depends on `fr-repl` in Cargo, so verification topology must include replication contract checks.
3. `fr-expire` is present in workspace but not yet consumed by `fr-runtime`/`fr-store`; this is intentional staging but must be called out as a current coupling gap, not omitted.

These checks were reconciled against current manifests and source entrypoints in this session.

## 15. DOC-PASS-02 Symbol/API Census and Surface Classification

### 15.1 Census method and scope

Scope for this pass:
1. All `pub` items in `crates/*/src/lib.rs` (current crate entrypoint surfaces).
2. Critical private helper functions that shape wire-visible behavior.
3. Surface classification by contract tier:
   - `public_contract`: directly consumed across crates or user-visible behavior boundary.
   - `internal_support`: private helper or crate-internal contract that still affects observable semantics.

Public symbol counts (current snapshot):

| Crate | `pub` symbol count | Primary exposure pattern |
|---|---:|---|
| `fr-command` | 3 | compact dispatch kernel API |
| `fr-config` | 24 | policy/config contract-rich crate |
| `fr-conformance` | 22 | harness and fixture orchestrator API |
| `fr-eventloop` | 14 | tick planning + replay/bootstrap API |
| `fr-expire` | 2 | minimal helper API |
| `fr-persist` | 4 | AOF stream boundary API |
| `fr-protocol` | 4 | RESP frame + parser API |
| `fr-repl` | 18 | replication FSM/decision API |
| `fr-runtime` | 4 | top-level runtime assembly API |
| `fr-store` | 3 | state container API |

### 15.2 Public contract surface (by crate)

| Crate | Public contract symbols | Classification | Regression risk |
|---|---|---|---|
| `fr-protocol` | `RespFrame`, `RespParseError`, `ParseResult`, `parse_frame` | `public_contract` | critical |
| `fr-command` | `CommandError`, `frame_to_argv`, `dispatch_argv` | `public_contract` | critical |
| `fr-store` | `Store`, `StoreError`, `PttlValue` | `public_contract` | critical |
| `fr-expire` | `ExpiryDecision`, `evaluate_expiry` | `public_contract` | medium |
| `fr-persist` | `AofRecord`, `PersistError`, `encode_aof_stream`, `decode_aof_stream` | `public_contract` | high |
| `fr-repl` | `HandshakeFsm`, `ReplProgress`, `decide_psync`, `evaluate_wait`, `evaluate_waitaof` (+ related structs/enums) | `public_contract` | high |
| `fr-eventloop` | `TickBudget`, `plan_tick`, `run_tick`, `replay_phase_trace`, `validate_bootstrap` (+ phase/bundle structs) | `public_contract` | high |
| `fr-config` | `RuntimePolicy`, `Mode`, threat/decision enums, TLS config/plan/rewrite validators | `public_contract` | high |
| `fr-runtime` | `Runtime`, `EvidenceEvent`, `EvidenceLedger`, `ecosystem` traits | `public_contract` | critical |
| `fr-conformance` | `HarnessConfig`, fixture/report structs, `run_*` harness entrypoints, `log_contract`, `phase2c_schema` modules | `public_contract` | high |

### 15.3 Internal-support surface affecting observable behavior

| Location | Symbol(s) | Why this is still contract-sensitive | Classification | Risk |
|---|---|---|---|---|
| `crates/fr-protocol/src/lib.rs` | `parse_frame_internal`, `read_line` | parse boundary and frame-completeness decisions determine protocol error classes | `internal_support` | critical |
| `crates/fr-command/src/lib.rs` | `parse_expire_options`, `apply_expiry_with_options` | option parsing and expiry gate semantics directly affect `EXPIRE*` replies | `internal_support` | critical |
| `crates/fr-store/src/lib.rs` | `drop_if_expired`, `glob_match` | lazy expiry and key pattern behavior affect `GET/TTL/KEYS/DBSIZE` observables | `internal_support` | high |
| `crates/fr-runtime/src/lib.rs` | `preflight_gate`, `handle_auth_command`, `handle_hello_command`, `record_threat_event` | admission policy and error/ledger mapping alter externally visible error paths and audit outputs | `internal_support` | critical |
| `crates/fr-conformance/src/lib.rs` | `validate_threat_expectation`, `validate_structured_log_emission` | defines pass/fail truth criteria for packet evidence and drift interpretation | `internal_support` | high |

### 15.4 High-regression interface watchlist

1. Wire contract watchlist:
   `fr-protocol::parse_frame` + private parser helpers and `fr-runtime::protocol_error_to_resp`.
2. Command semantics watchlist:
   `fr-command::dispatch_argv` + expiry option internals + `fr-store` mutation/ttl helpers.
3. Admission/security watchlist:
   `fr-runtime` auth/HELLO/cluster gate handlers (`handle_auth_command`, `handle_hello_command`, `preflight_gate`).
4. Verification truth-source watchlist:
   `fr-conformance::run_fixture`, `run_protocol_fixture`, `run_replay_fixture`, and threat/log validators.

Each watchlist surface should be mapped in downstream passes to unit IDs, e2e scenario IDs, and structured-log reason codes.

## 16. DOC-PASS-03 Data Model, State, and Invariant Mapping

### 16.1 Primary state-bearing models (current implementation)

| Model | Owner crate | Core fields/state | Mutation authority | Invalid-state handling |
|---|---|---|---|---|
| `RespFrame` | `fr-protocol` | tagged RESP2 frame variants (`SimpleString`, `Error`, `Integer`, `BulkString`, `Array`) | parser and encoder paths in `fr-protocol` | unknown/unsupported prefixes return parse errors, not partial frames |
| `Store` + `Entry` | `fr-store` | `HashMap<Vec<u8>, Entry>` where `Entry={value, expires_at_ms}` | `Store::*` mutation methods (`set`, `del`, `incr*`, `expire*`, `rename*`, `flushdb`) | expired keys are dropped lazily (`drop_if_expired`), missing keys return sentinel states instead of panics |
| `Runtime` | `fr-runtime` | policy + store + evidence ledger + TLS runtime state + auth state + cluster client state | `Runtime::execute_frame/execute_bytes` and config/auth/cluster handlers | protocol and command failures map to Redis error frames; invalid admission paths are rejected before dispatch |
| `AuthState` | `fr-runtime` (private) | `requirepass`, `authenticated_user` | `set_requirepass`, `handle_auth_command`, `handle_hello_command` | `requirepass` set forces unauthenticated state until valid credentials |
| `ClusterClientState` | `fr-runtime` (private) | `mode` (`ReadWrite`/`ReadOnly`), `asking` flag | `handle_asking_command`, `handle_readonly_command`, `handle_readwrite_command` | invalid arity or unknown subcommands return explicit errors |
| `EvidenceLedger` + `EvidenceEvent` | `fr-runtime` | append-only event list with hashes/reason codes/decision metadata | `record_threat_event`, `record_tls_config_event` | no event is emitted when policy disables ledger; otherwise event rows are deterministic and explicit |
| `TlsConfig` | `fr-config` | port/files/protocols/ciphers/auth-clients/operational knobs | config parse/validate/rewrite routines | malformed values return typed `TlsCfgError` reason codes |
| `TlsRuntimeState` | `fr-config` + `fr-runtime` | active config + listener enable flags + connection-type config flag | `plan_tls_runtime_apply` and `Runtime::apply_tls_config` | invalid transitions fail with typed contract violations; strict/hardened policy gate decides handling |
| `ReplProgress` | `fr-repl` | replication state + primary/replica offsets | replication FSM/update helpers | replica ACK offset never regresses (monotonic enforcement) |
| `HandshakeFsm` | `fr-repl` | handshake state machine (`Init` -> `PingSeen` -> ... -> `Online`) | `on_step`, `on_psync_accepted` | illegal transitions return typed `ReplError` mismatch codes |

### 16.2 State transition maps (critical paths)

1. Runtime request state path:
   bytes -> parse (`fr-protocol`) -> preflight gate (`fr-runtime`) -> argv projection (`fr-command`) -> admission/auth/cluster gate (`fr-runtime`) -> command dispatch (`fr-command`) -> store mutation (`fr-store`) -> response frame.
2. Replication handshake path:
   `Init -> PingSeen -> (AuthSeen|ReplconfSeen) -> PsyncSent -> Online`; any illegal transition returns `repl.handshake_state_machine_mismatch` or `repl.fullresync_reply_parse_violation`.
3. TLS runtime apply path:
   candidate config -> validation -> apply plan (`Enable|Disable|Keep`) -> context swap + connection-type configure gating; invalid plan returns `TlsCfgError` with fail-closed semantics in strict mode.

### 16.3 Invariant catalog (with enforcement points)

| Invariant ID | Invariant | Enforcement point | Failure behavior |
|---|---|---|---|
| INV-PROT-001 | Parser emits complete frames only | `fr-protocol::parse_frame` via `Incomplete`/typed parse errors | fail-closed parse error mapping in runtime |
| INV-PROT-002 | Unsupported RESP3 prefixes are rejected explicitly | `RespParseError::UnsupportedResp3Type` | Redis-visible `ERR Protocol error: unsupported RESP3 type prefix` |
| INV-CMD-001 | Command argv must be array-like and non-empty | `fr-command::frame_to_argv` | `InvalidCommandFrame` -> protocol error response |
| INV-CMD-002 | Expiry option combinations are contract-valid (`NX/XX/GT/LT`) | `parse_expire_options` | syntax error, no partial mutation |
| INV-STORE-001 | Expired keys are never returned as live values | `drop_if_expired` on read/mutate paths | key treated as missing |
| INV-STORE-002 | Integer mutation never silently overflows | `StoreError::IntegerOverflow` checks | explicit overflow error response |
| INV-AUTH-001 | `requirepass` implies pre-dispatch auth gate | `Runtime::execute_frame` NOAUTH branch | command rejected before dispatch |
| INV-TLS-001 | TLS apply cannot bypass config and transition checks | `plan_tls_runtime_apply` + `validate_tls_config` | typed config violation; strict-mode fail-closed |
| INV-REPL-001 | Replica ACK offset monotonicity | `ReplProgress::ack_replica_offset` | lower ACK ignored |
| INV-REPL-002 | PSYNC decision is deterministic on replid/window | `decide_psync` | explicit full-resync rejection reason |
| INV-EVENT-001 | Event-loop phase order is deterministic | `replay_phase_trace` + `EVENT_LOOP_PHASE_ORDER` | typed phase replay error |

### 16.4 Sentinel values and boundary semantics

| Surface | Sentinel / boundary value | Meaning |
|---|---|---|
| `PTTL`/`TTL` | `-2` | key missing |
| `PTTL`/`TTL` | `-1` | key exists with no expiry |
| `expire*` with non-future deadline | immediate delete path | operation returns success but key is removed |
| `HELLO` protocol version not in `{2,3}` | `NOPROTO unsupported protocol version` | fail-closed for unknown protocol versions |
| `AUTH` when no password configured | `ERR AUTH <password> called without any password configured...` | explicit policy failure |
| `CLUSTER` unknown subcommand | `ERR Unknown subcommand...` | explicit bounded command-scope error |

### 16.5 Invalid-state handling posture

Invalid-state strategy in the current codebase is predominantly fail-closed:
1. parse/config/handshake violations return typed errors immediately.
2. mutation paths avoid partial writes on validation failure.
3. unknown or unsupported protocol/config shapes are rejected explicitly.
4. runtime evidence paths capture rejection reason codes when ledger emission is enabled.

## 17. DOC-PASS-04 Execution-Path Tracing and Control-Flow Narratives

### 17.1 Narrative cards index

| Path ID | Workflow | Primary entrypoint | Key branch/fallback points | Source anchors | Oracle anchors |
|---|---|---|---|---|---|
| EP-001 | Request bytes to reply bytes | `Runtime::execute_bytes` | parse success vs parse failure | `crates/fr-runtime/src/lib.rs:374`, `crates/fr-protocol/src/lib.rs:96`, `crates/fr-runtime/src/lib.rs:755` | `crates/fr-runtime/src/lib.rs:1095`, `crates/fr-runtime/src/lib.rs:1112` |
| EP-002 | Preflight compatibility gate | `Runtime::execute_frame` + `preflight_gate` | array/bulk cap rejection vs dispatch path | `crates/fr-runtime/src/lib.rs:285`, `crates/fr-runtime/src/lib.rs:522` | `crates/fr-runtime/src/lib.rs:1070`, `crates/fr-conformance/src/lib.rs:1427` |
| EP-003 | Auth/HELLO admission gate | `handle_auth_command` + `handle_hello_command` | auth success, wrongpass, noauth pre-dispatch rejection | `crates/fr-runtime/src/lib.rs:401`, `crates/fr-runtime/src/lib.rs:424`, `crates/fr-runtime/src/lib.rs:340` | `crates/fr-runtime/src/lib.rs:928`, `crates/fr-runtime/src/lib.rs:939`, `crates/fr-runtime/src/lib.rs:986` |
| EP-004 | Cluster client-mode control path | `handle_cluster_command`, `READONLY`, `READWRITE`, `ASKING` | supported HELP scaffold vs unknown subcommand, mode flag transitions | `crates/fr-runtime/src/lib.rs:499` | `crates/fr-runtime/src/lib.rs:998`, `crates/fr-runtime/src/lib.rs:1032` |
| EP-005 | Command dispatch and state mutation | `dispatch_argv` + store mutators | command parse/arity error, syntax fallback, store error mapping | `crates/fr-command/src/lib.rs:47`, `crates/fr-command/src/lib.rs:576`, `crates/fr-store/src/lib.rs:317` | `crates/fr-command/src/lib.rs:757`, `crates/fr-command/src/lib.rs:1175` |
| EP-006 | AOF replay assertion path | `run_replay_fixture` | decode fail, record replay, assertion mismatch | `crates/fr-conformance/src/lib.rs:445`, `crates/fr-persist/src/lib.rs:59` | `crates/fr-conformance/src/lib.rs:1023`, `crates/fr-conformance/src/lib.rs:1031` |
| EP-007 | Live oracle differential protocol path | `run_live_redis_protocol_diff` | TCP connect/read failure, parse mismatch, log persistence fallback | `crates/fr-conformance/src/lib.rs:354`, `crates/fr-conformance/src/lib.rs:628` | `crates/fr-conformance/src/lib.rs:1749` |
| EP-008 | Event-loop deterministic planning path | runtime wrappers over `fr-eventloop` | normal vs blocked mode, phase replay acceptance vs rejection | `crates/fr-runtime/src/lib.rs:167`, `crates/fr-runtime/src/lib.rs:200`, `crates/fr-eventloop/src/lib.rs:211` | `crates/fr-runtime/src/lib.rs:819`, `crates/fr-runtime/src/lib.rs:883` |
| EP-009 | Runtime evidence to structured log path | `record_threat_event` -> `validate_structured_log_emission` | emission disabled, conversion validation failure, append path | `crates/fr-runtime/src/lib.rs:587`, `crates/fr-conformance/src/lib.rs:804`, `crates/fr-conformance/src/log_contract.rs:67` | `crates/fr-conformance/src/lib.rs:954`, `crates/fr-conformance/src/lib.rs:988`, `crates/fr-conformance/src/log_contract.rs:474` |

### 17.2 EP-001 Request bytes -> runtime response narrative

1. `Runtime::execute_bytes` computes input digest/state digest and calls `parse_frame`.
2. Parse success branch:
   decoded frame is forwarded to `Runtime::execute_frame`, then response is encoded to bytes.
3. Parse failure branch:
   `protocol_error_to_resp` maps parse error class to explicit Redis error text and `record_threat_event` emits `reason_code=protocol_parse_failure`.
4. Fail-safe property:
   malformed input does not reach command dispatch/store mutation.

Replay anchors:
- `rch exec -- cargo test -p fr-runtime -- --nocapture protocol_invalid_bulk_length_error_string`
- `rch exec -- cargo test -p fr-runtime -- --nocapture protocol_unsupported_resp3_type_error_string`

### 17.3 EP-002 Compatibility preflight narrative

1. `execute_frame` calls `preflight_gate` before argv projection and command dispatch.
2. Branch A (`compat_array_len_exceeded`):
   oversized array returns immediate protocol error and records `ThreatClass::ResourceExhaustion`.
3. Branch B (`compat_bulk_len_exceeded`):
   oversized bulk payload returns immediate protocol error and records equivalent resource clamp reasoning.
4. Pass branch:
   command continues to argv projection and dispatch.

Replay anchors:
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u005_noauth_gate_runs_before_dispatch`
- `rch exec -- cargo test -p fr-runtime -- --nocapture compatibility_gate_trips_on_large_array`

### 17.4 EP-003 Authentication and HELLO control-flow narrative

1. `execute_frame` fast-paths `AUTH` and `HELLO` before generic noauth gate.
2. `AUTH` branches:
   wrong arity -> standard wrong-arity error; no configured password -> explicit config error; valid credentials -> authenticated state promotion.
3. `HELLO` branches:
   invalid protocol version -> `NOPROTO`; `AUTH` option with wrong creds -> wrongpass; no auth supplied while auth required -> `NOAUTH`.
4. Generic noauth branch:
   any non-auth command while unauthenticated is rejected pre-dispatch with `reason_code=auth.noauth_gate_violation`.

Replay anchors:
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u002_auth_success_transitions_state`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u003_auth_wrongpass_rejected_without_state_promotion`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u004_hello_auth_early_fails_and_success_path_authenticates`

### 17.5 EP-004 Cluster mode and subcommand routing narrative

1. Cluster-related commands are handled inside runtime admission path, not inside `dispatch_argv`.
2. `CLUSTER` path supports deterministic `HELP` scaffold; unsupported subcommands return explicit bounded error text.
3. `READONLY`, `READWRITE`, and `ASKING` mutate per-client mode flags:
   `READWRITE` clears `asking`; `ASKING` sets temporary flag.
4. Invalid arity or unknown subcommands never mutate mode state.

Replay anchors:
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_007_u001_cluster_subcommand_router_is_deterministic`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_007_u007_client_cluster_mode_flags_transition_cleanly`

### 17.6 EP-005 Command dispatch and mutation narrative

1. `dispatch_argv` resolves command by case-insensitive comparisons and routes to command-specific handlers.
2. Control branches:
   wrong arity, syntax errors, invalid integer, and unknown command all return deterministic error mapping via `command_error_to_resp`.
3. Expiry path:
   `parse_expire_options` enforces option constraints (`NX/XX/GT/LT`) before mutation; invalid combos fail without partial state change.
4. Store mutation path:
   `Store` methods apply lazy expiry (`drop_if_expired`) before reads/mutations to preserve key visibility invariants.

Replay anchors:
- `rch exec -- cargo test -p fr-command -- --nocapture set_with_ex_option`
- `rch exec -- cargo test -p fr-command -- --nocapture expire_option_compatibility_rules_match_redis`
- `rch exec -- cargo test -p fr-store -- --nocapture expire_and_pttl`

### 17.7 EP-006 Replay fixture narrative (AOF stream -> assertions)

1. `run_replay_fixture` loads fixture records and encodes stream via `encode_aof_stream`.
2. Decode branch:
   `decode_aof_stream` failure produces explicit failed case outcome (`AOF stream decode failed`) and stops that case branch.
3. Replay branch:
   each decoded record is executed through runtime, then assertions are checked with explicit pass/fail outcomes.
4. Logging branch:
   structured-log emission validation runs for each replayed record and assertion path.

Replay anchors:
- `rch exec -- cargo test -p fr-conformance -- --nocapture conformance_replay_fixture_passes`
- `rch exec -- cargo test -p fr-conformance -- --nocapture run_replay_fixture_allows_structured_log_persistence_toggle`

### 17.8 EP-007 Live oracle differential protocol narrative

1. `run_live_redis_protocol_diff` establishes live Redis connection, writes raw protocol payload, and reads oracle response.
2. Runtime path for same payload executes via `Runtime::execute_bytes`; parsed runtime response is compared against live oracle response.
3. Branch points:
   network connect/read/write failure, oracle invalid RESP, runtime mismatch, or structured-log emission failure.
4. Failure handling:
   case is marked failed with explicit expected/actual frames and detail, preserving deterministic diagnostics.

Replay anchor:
- `rch exec -- cargo test -p fr-conformance -- --nocapture live_redis_protocol_negative_matches_runtime`

### 17.9 EP-008 Event-loop deterministic planning narrative

1. Runtime wrappers call into `fr-eventloop` for tick planning and phase replay checks.
2. Planning branch:
   `Normal` mode uses configured budget; `Blocked` mode clamps accepts/commands and poll timeout semantics.
3. Replay branch:
   valid phase sequence returns completed ticks; invalid start/transition yields typed replay errors.
4. Bootstrap branch:
   missing before/after hooks or server cron timer yields explicit bootstrap contract errors.

Replay anchors:
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_001_u001_runtime_exposes_deterministic_phase_order`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_001_u011_runtime_phase_replay_accepts_contract_order`

### 17.10 EP-009 Structured-log conversion and persistence narrative

1. Runtime rejection events are recorded via `record_threat_event` with reason codes and replay metadata.
2. Conformance harness converts runtime evidence using `StructuredLogEvent::from_runtime_evidence`.
3. Persistence branch:
   `append_structured_log_jsonl` appends newline-delimited JSON logs when enabled.
4. Contract guarantees:
   `reason_code` and `replay_cmd` are required non-empty fields in structured-log validation.

Replay anchors:
- `rch exec -- cargo test -p fr-conformance -- --nocapture run_fixture_accepts_structured_log_persistence_toggle`
- `rch exec -- cargo test -p fr-conformance -- --nocapture run_protocol_fixture_persists_structured_logs_when_enabled`

## 18. DOC-PASS-05 Complexity, Performance, and Memory Characterization

### 18.1 Operation complexity map (current implementation)

| Surface | Representative path | Time complexity (dominant term) | Space behavior | Source anchors |
|---|---|---|---|---|
| RESP frame parse | recursive frame parse over input bytes | `O(n)` in input size | `O(n)` for decoded frame buffers | `crates/fr-protocol/src/lib.rs:101`, `crates/fr-protocol/src/lib.rs:163` |
| Command dispatch resolution | command-name chain in `dispatch_argv` | `O(k)` over supported command checks | `O(1)` aside from reply payloads | `crates/fr-command/src/lib.rs:47` |
| Key read/write | `HashMap` lookup/insert in `Store` | expected `O(1)` average | key/value allocation proportional to payload size | `crates/fr-store/src/lib.rs:26`, `crates/fr-store/src/lib.rs:27` |
| Multi-key read | `mget` over input keys | `O(m)` in key count | `O(m)` output vector + clones | `crates/fr-store/src/lib.rs:188` |
| Key pattern scan | `keys_matching` + glob checks | `O(K * P)` for keys and pattern cost | `O(K)` key snapshot + matched result | `crates/fr-store/src/lib.rs:288` |
| Database size with lazy expiry | `dbsize` with full-key sweep | `O(K)` | `O(K)` key snapshot during sweep | `crates/fr-store/src/lib.rs:305` |
| AOF stream decode | cursor loop over concatenated frames | `O(n)` in stream bytes | `O(r)` decoded records + argv allocations | `crates/fr-persist/src/lib.rs:59` |
| Replay fixture execution | record loop + assertion loop | `O(r + a)` per case | `O(r + a)` case artifacts/reports | `crates/fr-conformance/src/lib.rs:445` |
| Event-loop plan | budget clamp and simple arithmetic | `O(1)` | `O(1)` | `crates/fr-eventloop/src/lib.rs:199`, `crates/fr-eventloop/src/lib.rs:211` |
| Replication wait evaluation | threshold checks on offsets slice | `O(N)` in replica count | `O(1)` | `crates/fr-repl/src/lib.rs:257` |

Legend:
- `n`: bytes in payload/stream, `k`: command variants, `K`: key cardinality, `P`: pattern-evaluation cost, `m`: requested keys, `r`: replay records, `a`: replay assertions, `N`: replica offsets.

### 18.2 Throughput-limiting paths and contention candidates

1. Command-name linear resolution in `dispatch_argv` is currently string-compare heavy and grows with command surface.
2. `KEYS`/`DBSIZE` paths perform full-key sweeps and can become latency spikes at higher key cardinalities.
3. Replay verification (`run_replay_fixture`) compounds stream decode + command execution + assertion checks and scales with fixture size.
4. RESP parse path allocates per-frame vectors and bulk buffers; malformed-load paths still pay scan cost before fail-closed return.

### 18.3 Memory growth vectors

| Component | Primary growth driver | Bound behavior today | Risk note |
|---|---|---|---|
| `Store.entries` | number of keys and value sizes | unbounded by policy in current code | high-cardinality workloads can increase RSS rapidly |
| RESP decoded frames | payload size and array depth | bounded at runtime by compatibility gate (`max_array_len`, `max_bulk_len`) | gate values must be tuned to prevent hostile memory pressure |
| Evidence ledger (`events: Vec`) | number of rejected/threat events | append-only while runtime instance lives | long-lived runtimes with ledger enabled can accumulate large audit vectors |
| Replay decoded records | fixture record count | per-case vectors in harness | very large fixtures may inflate transient memory during replay validation |
| Structured log aggregation | `Vec` of converted events prior to append | proportional to new events in case batch | bounded in normal test flows, but burst failures can enlarge transient allocations |

Source anchors:
- `crates/fr-store/src/lib.rs:27`
- `crates/fr-runtime/src/lib.rs:105`
- `crates/fr-runtime/src/lib.rs:522`
- `crates/fr-conformance/src/lib.rs:445`
- `crates/fr-conformance/src/lib.rs:809`

### 18.4 Performance envelope and fallback behavior

1. Strict mode defaults to fail-closed behavior on incompatible protocol/config surfaces; this preserves semantic safety but can increase rejection-path overhead under hostile traffic.
2. Hardened mode allows bounded defenses only through allowlisted deviation categories in `RuntimePolicy`; non-allowlisted deviations are rejected with explicit reason codes.
3. Event-loop blocked mode enforces reduced accept/command budgets (`TickBudget::bounded_for_blocked_mode`) to constrain worst-case cycle pressure.

### 18.5 Measurement and replay command anchors (`rch` only)

Baseline and static quality gates:
- `rch exec -- cargo fmt --check`
- `rch exec -- cargo check --workspace --all-targets`
- `rch exec -- cargo clippy --workspace --all-targets -- -D warnings`

Behavior and conformance probes:
- `rch exec -- cargo test -p fr-runtime -- --nocapture`
- `rch exec -- cargo test -p fr-command -- --nocapture`
- `rch exec -- cargo test -p fr-store -- --nocapture`
- `rch exec -- cargo test -p fr-conformance -- --nocapture`

Targeted replay/diff probes:
- `rch exec -- cargo test -p fr-conformance -- --nocapture conformance_replay_fixture_passes`
- `rch exec -- cargo test -p fr-conformance -- --nocapture live_redis_protocol_negative_matches_runtime`

Existing baseline artifact anchors:
- `baselines/round1_conformance_baseline.json`
- `baselines/round2_protocol_negative_baseline.json`
- `baselines/round1_conformance_strace.txt`
- `baselines/round2_protocol_negative_strace.txt`

## 19. DOC-PASS-06 Concurrency/Lifecycle Semantics and Ordering Guarantees

### 19.1 Concurrency model (current code)

1. Runtime command execution is effectively single-threaded at the `Runtime` instance level: one frame is parsed/gated/dispatched at a time through `execute_frame`/`execute_bytes`.
2. Event-loop semantics are modeled as deterministic phase progression rather than concurrent worker execution.
3. Conformance harness may open sockets and iterate fixtures, but each per-case runtime state transition is serial within a case execution path.

### 19.2 Lifecycle phases

| Lifecycle slice | Primary state transitions | Ordering contract | Source anchors |
|---|---|---|---|
| Event-loop cycle | `BeforeSleep -> Poll -> FileDispatch -> TimeDispatch -> AfterSleep` | order must match constant phase sequence | `crates/fr-eventloop/src/lib.rs:46`, `crates/fr-eventloop/src/lib.rs:143` |
| Runtime request | parse -> preflight -> auth/cluster admission -> dispatch -> reply | admission gates run before mutation/dispatch | `crates/fr-runtime/src/lib.rs:285`, `crates/fr-runtime/src/lib.rs:522`, `crates/fr-runtime/src/lib.rs:331` |
| Auth lifecycle | bootstrap auth state -> credential check -> authenticated session | `NOAUTH` rejection occurs before generic dispatch when required | `crates/fr-runtime/src/lib.rs:401`, `crates/fr-runtime/src/lib.rs:986` |
| Replay lifecycle | AOF decode order -> record replay order -> assertion order | record and assertion evaluation are deterministic loop order | `crates/fr-persist/src/lib.rs:62`, `crates/fr-conformance/src/lib.rs:488`, `crates/fr-conformance/src/lib.rs:519` |
| Replication lifecycle | handshake progression + monotonic offsets | illegal transitions rejected; ACK offsets never regress | `crates/fr-repl/src/lib.rs:97`, `crates/fr-repl/src/lib.rs:43` |

### 19.3 Ordering guarantees (explicit)

| Guarantee ID | Guarantee | Enforcement mechanism | Failure mode |
|---|---|---|---|
| ORD-001 | Event-loop phase order is deterministic | `EVENT_LOOP_PHASE_ORDER` + `replay_phase_trace` expected-next checks | typed `PhaseReplayError` on mismatch |
| ORD-002 | Noauth gate runs before command dispatch | `execute_frame` checks `requires_auth` before `dispatch_argv` | immediate `NOAUTH` error |
| ORD-003 | Replay applies records in stream order | `decode_aof_stream` cursor walk + sequential `for` replay loop | replay case failure with deterministic case naming |
| ORD-004 | Replica ACK offsets are monotonic | `ack_replica_offset` only promotes higher offsets | lower ACK ignored, no state regression |
| ORD-005 | PSYNC decision is deterministic on `(replid, offset window)` | `decide_psync` pure decision function | explicit full-resync rejection reason |
| ORD-006 | Blocked-mode event loop budget is clamped | `bounded_for_blocked_mode` and blocked poll-timeout branch | bounded throughput rather than unbounded cycle work |

### 19.4 Lifecycle branch/fallback semantics

1. Event-loop replay:
   invalid start phase, invalid transition, or partial tick all return explicit replay errors rather than inferring continuation.
2. Runtime admission:
   invalid command frame, protocol parse failure, and auth-policy violations return fail-closed error replies and do not mutate store state.
3. Replay harness:
   decode failure branch surfaces explicit assertion details and halts that case branch safely.
4. Replication FSM:
   unexpected handshake step or PSYNC reply state mismatch returns typed `ReplError` reason codes instead of implicit recovery.

### 19.5 Verification anchors and replay commands (`rch`)

Event-loop ordering:
- `rch exec -- cargo test -p fr-eventloop -- --nocapture fr_p2c_001_u001_phase_order_is_deterministic`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_001_u011_runtime_phase_replay_accepts_contract_order`

Admission ordering:
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u005_noauth_gate_runs_before_dispatch`

Replay ordering:
- `rch exec -- cargo test -p fr-conformance -- --nocapture conformance_replay_fixture_passes`
- `rch exec -- cargo test -p fr-conformance -- --nocapture run_replay_fixture_allows_structured_log_persistence_toggle`

Replication ordering:
- `rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u004_replica_ack_offsets_never_regress`
- `rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u001_psync_accepts_partial_resync_inside_window`
- `rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u005_wait_threshold_counts_acknowledged_replicas`

## 20. DOC-PASS-07 Error Taxonomy, Failure Modes, and Recovery Semantics

### 20.1 Cross-crate error surface inventory

| Crate | Primary error types | Error-to-contract bridge | Source anchors |
|---|---|---|---|
| `fr-protocol` | `RespParseError` (`Incomplete`, `InvalidPrefix`, `UnsupportedResp3Type`, `InvalidInteger`, `InvalidBulkLength`, `InvalidMultibulkLength`, `InvalidUtf8`) | surfaced as protocol error replies via runtime translation | `crates/fr-protocol/src/lib.rs:68`, `crates/fr-runtime/src/lib.rs:755` |
| `fr-command` | `CommandError` (`InvalidCommandFrame`, `InvalidUtf8Argument`, `UnknownCommand`, `WrongArity`, `InvalidInteger`, `SyntaxError`, `NoSuchKey`, `Store`) | surfaced as deterministic `ERR ...` replies in runtime | `crates/fr-command/src/lib.rs:7`, `crates/fr-runtime/src/lib.rs:715` |
| `fr-store` | `StoreError` (`ValueNotInteger`, `IntegerOverflow`, `KeyNotFound`) | mapped into `CommandError::Store` and then wire errors | `crates/fr-store/src/lib.rs:6`, `crates/fr-runtime/src/lib.rs:743` |
| `fr-persist` | `PersistError` (`InvalidFrame`, `Parse(RespParseError)`) | fail decode/replay branch on first invalid frame | `crates/fr-persist/src/lib.rs:11`, `crates/fr-persist/src/lib.rs:59` |
| `fr-repl` | `ReplError`, `PsyncRejection` | explicit reason-code methods for contract checks | `crates/fr-repl/src/lib.rs:76`, `crates/fr-repl/src/lib.rs:176` |
| `fr-eventloop` | `PhaseReplayError`, `BootstrapError` | explicit reason-code methods for deterministic ordering/bootstrap failures | `crates/fr-eventloop/src/lib.rs:55`, `crates/fr-eventloop/src/lib.rs:101` |
| `fr-config` | `TlsCfgError` variant family | `reason_code()` projection for strict/hardened policy auditing | `crates/fr-config/src/lib.rs:241`, `crates/fr-config/src/lib.rs:261` |
| `fr-runtime` | threat-event reason codes + translated user-facing replies | fail-closed admission gates + evidence ledger | `crates/fr-runtime/src/lib.rs:129`, `crates/fr-runtime/src/lib.rs:587` |
| `fr-conformance` | expectation mismatches and structured-log validation errors (`String`) | verifies reason-code contracts and deterministic threat semantics | `crates/fr-conformance/src/lib.rs:672`, `crates/fr-conformance/src/log_contract.rs:102` |

### 20.2 User-visible reply taxonomy (wire contract)

| Class | Representative trigger | Reply shape / message family | Source anchors |
|---|---|---|---|
| Protocol parse failure | malformed RESP frame (`invalid bulk length`, `invalid prefix`, EOF) | `ERR Protocol error: ...` | `crates/fr-runtime/src/lib.rs:755` |
| Frame-to-command projection failure | non-array / invalid argv item shape | `ERR Protocol error: invalid command frame` (pre-dispatch) | `crates/fr-runtime/src/lib.rs:296` |
| Command routing failure | unknown command or wrong arity | `ERR unknown command ...`, `ERR wrong number of arguments for ...` | `crates/fr-runtime/src/lib.rs:723`, `crates/fr-runtime/src/lib.rs:734` |
| Argument semantic failure | bad integer / syntax | `ERR value is not an integer or out of range`, `ERR syntax error` | `crates/fr-runtime/src/lib.rs:738`, `crates/fr-runtime/src/lib.rs:741` |
| Store-domain violation | overflow or no such key | `ERR increment or decrement would overflow`, `ERR no such key` | `crates/fr-runtime/src/lib.rs:747`, `crates/fr-runtime/src/lib.rs:750` |
| Auth admission failure | unauthenticated command under `requirepass` | `NOAUTH Authentication required.` | `crates/fr-runtime/src/lib.rs:20`, `crates/fr-runtime/src/lib.rs:331` |
| Credential mismatch | invalid `AUTH`/`HELLO AUTH` tuple | `WRONGPASS ...` | `crates/fr-runtime/src/lib.rs:21`, `crates/fr-runtime/src/lib.rs:463` |
| HELLO protocol mismatch | unsupported protocol version | `NOPROTO unsupported protocol version ...` | `crates/fr-runtime/src/lib.rs:434` |
| Compatibility-gate clamp | array/bulk exceeds policy gate | `ERR Protocol error: ... exceeds compatibility gate` | `crates/fr-runtime/src/lib.rs:533`, `crates/fr-runtime/src/lib.rs:559` |
| Cluster stub invalid subcommand | unsupported `CLUSTER` route in current stage | `ERR Unknown subcommand or wrong number of arguments for 'CLUSTER'...` | `crates/fr-runtime/src/lib.rs:23`, `crates/fr-runtime/src/lib.rs:519` |

### 20.3 Deterministic reason-code namespaces

| Namespace | Emission source | Canonical examples | Contract role |
|---|---|---|---|
| `protocol_*` / parser-gate | runtime threat recording on parse failure | `protocol_parse_failure`, `invalid_command_frame` | parser abuse detection and replayability |
| `compat_*` | runtime compatibility gate | `compat_array_len_exceeded`, `compat_bulk_len_exceeded` | resource-clamp fail-closed boundary |
| `auth.*` | runtime auth admission gate | `auth.noauth_gate_violation` | pre-dispatch auth ordering invariant |
| `eventloop.*` | event-loop replay/bootstrap errors | `eventloop.main_loop_entry_missing`, `eventloop.dispatch.stage_transition_invalid`, `eventloop.dispatch.order_mismatch`, `eventloop.hook_install_missing` | deterministic loop-order and bootstrap guarantees |
| `repl.*` | replication FSM and PSYNC decision logic | `repl.handshake_state_machine_mismatch`, `repl.fullresync_reply_parse_violation`, `repl.psync_replid_or_offset_reject_mismatch`, `repl.psync_fullresync_fallback_mismatch` | replication safety/fallback semantics |
| `tlscfg.*` / `tls.*` | TLS config validation + runtime apply policy | `tlscfg.protocols_parse_contract_violation`, `tlscfg.safety_gate_contract_violation`, `tlscfg.hardened_nonallowlisted_rejected`, `tls.handshake_verify_policy_violation` | config hardening and downgrade-abuse defense |
| `parity_ok` / `journey_ok` | log-contract golden artifacts | structured log reason codes for success paths | deterministic harness log schema validation |

Source anchors:
- `crates/fr-runtime/src/lib.rs:383`
- `crates/fr-runtime/src/lib.rs:537`
- `crates/fr-eventloop/src/lib.rs:69`
- `crates/fr-repl/src/lib.rs:86`
- `crates/fr-config/src/lib.rs:261`
- `crates/fr-conformance/src/log_contract.rs:202`

### 20.4 Failure-mode and recovery matrix

| Failure mode | Detection point | Deterministic response | State mutation posture | Recovery semantics |
|---|---|---|---|---|
| Invalid/incomplete RESP request | `parse_frame`/runtime parse branch | protocol error reply + threat event (`protocol_parse_failure`) | no command dispatch; store remains unchanged | client must resend valid frame; server stays online |
| Invalid command frame shape | `frame_to_argv` in `execute_frame` | reject frame (`invalid_command_frame`) | pre-dispatch rejection; no store mutation | fail-closed at admission boundary |
| Unauthenticated command under `requirepass` | runtime auth gate before dispatch | `NOAUTH` + `auth.noauth_gate_violation` evidence | command not executed | explicit AUTH required before progress |
| Oversized array/bulk vs compatibility gate | runtime `preflight_gate` | compatibility-gate rejection + threat event | no command dispatch | strict fail-closed in strict mode; bounded defensive classification in hardened mode |
| TLS config contract violation | `plan_tls_runtime_apply` / `validate_tls_config` | return `TlsCfgError` and record `tls_config` threat event | runtime TLS state remains unchanged on failed plan/apply path | operator must provide valid config; hardened mode may reclassify non-allowlisted deviations to rejection errors |
| Event-loop phase trace invalid | `replay_phase_trace` | typed `PhaseReplayError` with reason code | no hidden repair; trace rejected | caller must supply valid phase order |
| Missing event-loop hooks/timer | `validate_bootstrap` | typed `BootstrapError` with reason code | bootstrap fails cleanly | install missing hooks/timer before run |
| Replication handshake order violation | `HandshakeFsm::on_step` | `ReplError::HandshakeStateMachineMismatch` | FSM state does not advance on invalid transition | retry with correct ordered steps |
| PSYNC mismatch/out-of-range | `decide_psync` | deterministic full-resync fallback with rejection reason | no unsafe partial-resync continuation | safe fallback to full resync |
| AOF decode invalid frame | `decode_aof_stream` | `PersistError::InvalidFrame` / parse error | replay stream processing halts | input stream must be repaired/replaced before replay |
| Threat expectation mismatch in conformance | `validate_threat_expectation` | explicit mismatch error with first offending detail | test case fails; no hidden reclassification | fix runtime reason-code or fixture expectation |
| Structured log schema violation | `StructuredLogEvent::validate` / persistence | deterministic error (`artifact_refs must not be empty`, etc.) | log write is aborted for invalid payload | fix event payload/schema inputs |

### 20.5 Strict-vs-hardened recovery policy boundaries

1. Strict mode:
   all threats resolve to `DecisionAction::FailClosed` + severity `S0`, with no behavior-altering recovery paths.
2. Hardened mode:
   explicit allowlisted deviation categories may take `BoundedDefense` (`S1`), while non-allowlisted deviations are rejected (`RejectNonAllowlisted`, `S2`).
3. Config downgrade handling:
   `evaluate_tls_hardened_deviation` enforces allowlist policy and promotes non-allowlisted deviations to `tlscfg.hardened_nonallowlisted_rejected`.
4. In both modes:
   error paths are deterministic and evidence-first; no silent fallback that alters external command semantics.

Source anchors:
- `crates/fr-config/src/lib.rs:107`
- `crates/fr-config/src/lib.rs:651`
- `crates/fr-runtime/src/lib.rs:625`

### 20.6 Recovery semantics by subsystem (what is and is not recoverable)

Recoverable (caller/operator can retry with corrected input/state):
- protocol frame shape/content errors after client resubmission,
- auth failures after successful `AUTH`,
- handshake FSM progression errors after step-order correction,
- PSYNC rejections via deterministic full-resync fallback,
- TLS directive/config violations after corrected config material.

Non-recoverable in-place (must fail closed immediately in current attempt):
- non-allowlisted hardened deviations,
- incompatible command-frame or gate violations in the active request packet,
- invalid event-loop replay traces/bootstrap prerequisites,
- invalid AOF frame encountered during decode cursor walk.

### 20.7 Evidence and conformance coupling for failures

| Contract plane | What is asserted | Source anchors |
|---|---|---|
| Runtime evidence ledger | every threat event captures `threat_class`, `decision_action`, `reason_code`, digests, replay command, artifacts | `crates/fr-runtime/src/lib.rs:597` |
| Conformance threat checks | optional expected threat blocks verify class, severity, decision, reason code, subsystem, and action | `crates/fr-conformance/src/lib.rs:713` |
| Structured log contract | conversion + schema validation requires non-empty reason code/replay command/artifacts | `crates/fr-conformance/src/log_contract.rs:103`, `crates/fr-conformance/src/log_contract.rs:144` |
| Replication reason-code vectors | handshake and PSYNC adversarial vectors assert expected reason-code mapping | `crates/fr-conformance/src/lib.rs:1056`, `crates/fr-conformance/src/lib.rs:1152` |

### 20.8 Verification anchors and replay commands (`rch`)

Error mapping and parser/admission behavior:
- `rch exec -- cargo test -p fr-runtime -- --nocapture protocol_invalid_bulk_length_error_string`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u005_noauth_gate_runs_before_dispatch`
- `rch exec -- cargo test -p fr-runtime -- --nocapture compatibility_gate_trips_on_large_array`

Event-loop/bootstrap reason-code behavior:
- `rch exec -- cargo test -p fr-eventloop -- --nocapture fr_p2c_001_u001_phase_order_is_deterministic`
- `rch exec -- cargo test -p fr-eventloop -- --nocapture fr_p2c_001_u010_bootstrap_requires_hooks_and_server_cron`

Replication rejection reason-code behavior:
- `rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u003_handshake_requires_ping_first`
- `rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u002_psync_rejects_replid_mismatch`
- `rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_handshake_contract_vectors_are_enforced`

TLS config fail-closed and hardened policy behavior:
- `rch exec -- cargo test -p fr-config -- --nocapture fr_p2c_009_u013_hardened_gate_rejects_non_allowlisted_deviation`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u013_hardened_non_allowlisted_tls_deviation_is_rejected`
- `rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract`

Structured logging and failure-evidence validity:
- `rch exec -- cargo test -p fr-conformance -- --nocapture runtime_evidence_conversion_rejects_empty_artifact_refs`
- `rch exec -- cargo test -p fr-conformance -- --nocapture threat_expectation_rejects_unexpected_event`

## 21. DOC-PASS-08 Security/Compatibility Edge Cases and Undefined Zones

### 21.1 Security/compatibility doctrine anchors (project-level)

1. Strict/hardened split and default fail-closed posture are explicit project doctrine:
   strict mode preserves observable compatibility; hardened mode allows only bounded allowlisted defenses.
2. Unknown or incompatible paths must fail closed unless explicitly allowlisted with evidence.
3. Threat handling requires deterministic reason-code emission plus replay/evidence metadata.

Source anchors:
- `COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md:59`
- `COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md:71`
- `SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md:8`
- `SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md:22`
- `SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md:96`

### 21.2 Edge-case controls currently implemented in code

| Edge ID | Surface | Trigger / hostile condition | Current strict behavior | Hardened behavior boundary | Source anchors |
|---|---|---|---|---|---|
| EC-001 | RESP parser | invalid prefix, unsupported RESP3 type, malformed lengths, incomplete frame | deterministic protocol error replies; fail before dispatch | no semantic relaxation; only bounded diagnostics class is permitted | `crates/fr-protocol/src/lib.rs:68`, `crates/fr-runtime/src/lib.rs:755`, `crates/fr-config/src/lib.rs:50` |
| EC-002 | Compatibility gate | command array/bulk exceeds configured thresholds | immediate fail-closed rejection with reason code (`compat_array_len_exceeded`, `compat_bulk_len_exceeded`) | may classify as bounded `ResourceClamp`; non-allowlisted remains reject | `crates/fr-runtime/src/lib.rs:533`, `crates/fr-runtime/src/lib.rs:567`, `crates/fr-config/src/lib.rs:118` |
| EC-003 | Auth admission | unauthenticated command path under `requirepass` | `NOAUTH` returned pre-dispatch with threat evidence | same outward gate semantics | `crates/fr-runtime/src/lib.rs:331`, `crates/fr-runtime/src/lib.rs:340` |
| EC-004 | TLS/config boundary | invalid config graph or unsafe transition | returns typed `TlsCfgError`, records config-downgrade threat event | allowlist gate enforced; non-allowlisted deviation maps to hardened rejection reason codes | `crates/fr-config/src/lib.rs:241`, `crates/fr-config/src/lib.rs:651`, `crates/fr-runtime/src/lib.rs:625` |
| EC-005 | Event-loop order/bootstrap | invalid phase trace or missing hooks/timer | deterministic typed replay/bootstrap errors with reason codes | no bounded semantic bypass path | `crates/fr-eventloop/src/lib.rs:55`, `crates/fr-eventloop/src/lib.rs:101`, `crates/fr-runtime/src/lib.rs:200` |
| EC-006 | Replication safety reducers | invalid handshake order, PSYNC replid/offset mismatch | invalid transitions rejected; PSYNC mismatch falls back to full-resync decision | no non-allowlisted path to unsafe continuation | `crates/fr-repl/src/lib.rs:76`, `crates/fr-repl/src/lib.rs:183`, `crates/fr-repl/src/lib.rs:198` |
| EC-007 | Threat-evidence/log contract | missing/mismatched threat expectations or malformed structured log payload | conformance case fails with explicit mismatch/validation errors | same contract in both modes | `crates/fr-conformance/src/lib.rs:672`, `crates/fr-conformance/src/lib.rs:713`, `crates/fr-conformance/src/log_contract.rs:103` |

### 21.3 Undefined and ambiguous zones (current gap register)

| Zone ID | Zone summary | Why this is security/compat sensitive | Current state in Rust | Contract/evidence anchor |
|---|---|---|---|---|
| UZ-001 | ACL/authorization parity beyond basic auth | startup source ambiguity, selector grammar, and ACL command semantics are critical policy boundaries | runtime has basic `AUTH`/`HELLO AUTH` gate, but ACL command family/selector model and transactional ACL load surfaces are missing | `crates/fr-runtime/src/lib.rs:401`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/legacy_anchor_map.md:20`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/legacy_anchor_map.md:35` |
| UZ-002 | Persistence manifest/replay/RDB/rewrite pipeline parity | replay ordering and tamper handling are core non-regression contracts | `fr-persist` currently provides minimal RESP AOF record encode/decode substrate only; manifest orchestration/rewrite/RDB/WAITAOF semantics remain missing | `crates/fr-persist/src/lib.rs:11`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/legacy_anchor_map.md:20`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/legacy_anchor_map.md:44` |
| UZ-003 | Active expire + eviction policy engine | resource-exhaustion and TTL/eviction ordering must stay deterministic and fail-closed under stress | lazy per-key expiry exists; active-expire cycles, maxmemory accounting, eviction loops/safety gates are not yet implemented | `crates/fr-expire/src/lib.rs:10`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/implementation_plan.md:32`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/legacy_anchor_map.md:21` |
| UZ-004 | Full replication command/runtime surface | handshake/order attacks and stale-stream replay depend on complete state-machine plumbing | core FSM and PSYNC decision helpers exist, but command surfaces (`SYNC/PSYNC/REPLCONF/WAIT/WAITAOF`) and cron/wake integrations are missing | `crates/fr-repl/src/lib.rs:97`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-006/legacy_anchor_map.md:25`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-006/legacy_anchor_map.md:38` |
| UZ-005 | Cluster route/failover/admin parity | slot ownership, redirect shaping, and cluster bus parsing are high-risk correctness/security boundaries | runtime currently exposes only minimal `CLUSTER HELP` scaffold and client mode toggles; slot/routing/failover/bus/admin surfaces remain undefined in Rust | `crates/fr-runtime/src/lib.rs:499`, `crates/fr-runtime/src/lib.rs:519`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-007/legacy_anchor_map.md:22` |
| UZ-006 | TLS runtime handshake/listener orchestration parity | downgrade/conflicting-config paths require deterministic fail-closed runtime behavior | config validation and policy gates exist, but runtime handshake/listener parity and persistence-linked rewrite paths are still contractual | `crates/fr-config/src/lib.rs:460`, `crates/fr-runtime/src/lib.rs:239`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-009/risk_note.md:145` |
| UZ-007 | Packet-specific hardened exhaustion reason-code coverage | contract tables require explicit `*.hardened_budget_exhausted_failclosed` events | those reason-code families are present in packet contracts/risk notes but are not yet emitted in active runtime code paths | `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/contract_table.md:102`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-006/contract_table.md:103`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-009/contract_table.md:104` |

### 21.4 Ambiguity classes and required handling posture

| Ambiguity class | Example | Required posture |
|---|---|---|
| Legacy behavior missing in Rust | cluster packet parser/failover reducers | fail closed, emit deterministic reason code, keep packet blocked from promotion |
| Partial implementation with contractual intent | expire helper exists without scheduler integration | document as non-parity substrate; prohibit parity claims beyond implemented scope |
| Contract docs ahead of implementation | packet-level hardened exhaustion reason codes listed in risk/contract tables | track as explicit implementation debt; no silent fallback to undocumented behavior |
| Doc-vs-code drift | legacy map entries claiming missing `AUTH` while runtime contains minimal auth flow | mark as stale contract artifact and reconcile in packet artifacts before closure |

### 21.5 Priority order for edge-case closure (documentation gating view)

1. Policy-critical gates first:
   ACL/auth ambiguity, TLS downgrade surfaces, and replication/cluster state-machine ordering.
2. Persistence and expiration second:
   tamper/replay + eviction pressure surfaces with clear fail-closed boundaries.
3. Exhaustion/fallback reason-code completion:
   ensure packet-level `hardened_budget_exhausted_failclosed` contracts are concretely emitted where required.

### 21.6 Verification anchors and replay commands (`rch`)

Core fail-closed + policy checks already executable:
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u005_noauth_gate_runs_before_dispatch`
- `rch exec -- cargo test -p fr-runtime -- --nocapture compatibility_gate_trips_on_large_array`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u013_hardened_non_allowlisted_tls_deviation_is_rejected`
- `rch exec -- cargo test -p fr-config -- --nocapture fr_p2c_009_u013_hardened_gate_rejects_non_allowlisted_deviation`
- `rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract`

Gap-sensitive packet contract suites (must remain explicit until parity implementation lands):
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_004`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_005`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_006`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_007`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_009`

## 22. DOC-PASS-10 Pass-A Completion Draft for `EXISTING_REDIS_STRUCTURE.md`

### 22.1 Quantitative expansion snapshot

| Metric | Current value | Evidence command |
|---|---|---|
| Total lines | `795` | `wc -l EXISTING_REDIS_STRUCTURE.md` |
| Top-level sections (`##`) | `21` | `rg -n "^## " EXISTING_REDIS_STRUCTURE.md` |
| Subsections (`###`) | `48` | `rg -n "^### " EXISTING_REDIS_STRUCTURE.md` |
| Inline crate line anchors (`crates/...:line`) | `162` | `rg -o "crates/[A-Za-z0-9_./-]+:[0-9]+" EXISTING_REDIS_STRUCTURE.md \| wc -l` |

Interpretation:
- The document is now a source-anchored operational map rather than a high-level outline.
- Coverage spans architecture, ownership, invariants, control flow, performance, lifecycle, error taxonomy, and security/compatibility edge zones.

### 22.2 Pass-to-section traceability matrix (Pass A scope)

| Completed docs pass bead | Coverage in this file | Status |
|---|---|---|
| `bd-2wb.24.1` DOC-PASS-00 (baseline matrix + targets) | section `8` | complete |
| `bd-2wb.24.2` DOC-PASS-01 (cartography + ownership) | sections `9`–`14` | complete |
| `bd-2wb.24.3` DOC-PASS-02 (symbol/API census) | section `15` | complete |
| `bd-2wb.24.4` DOC-PASS-03 (state/invariant mapping) | section `16` | complete |
| `bd-2wb.24.5` DOC-PASS-04 (execution-path tracing) | section `17` | complete |
| `bd-2wb.24.6` DOC-PASS-05 (complexity/perf/memory) | section `18` | complete |
| `bd-2wb.24.7` DOC-PASS-06 (concurrency/lifecycle ordering) | section `19` | complete |
| `bd-2wb.24.8` DOC-PASS-07 (error taxonomy/failure/recovery) | section `20` | complete |
| `bd-2wb.24.9` DOC-PASS-08 (security/compat edge cases + undefined zones) | section `21` | complete |

### 22.3 Acceptance criteria evidence for `bd-2wb.24.11`

Criterion 1: materially expanded and section-complete against gap matrix
- Evidence:
  section growth from foundational map (`1`–`7`) through complete pass chain (`8`–`21`) with explicit per-pass artifact blocks.

Criterion 2: topology, ownership, and dependency explanations are clear
- Evidence:
  subsystem cartography and ownership boundaries (`9`–`12`) plus hidden-coupling and docs-vs-code conflict registers (`13`–`14`).

Criterion 3: structure claims are traceable to code/contracts
- Evidence:
  dense source-anchor usage (`162` crate line anchors) plus packet/contract anchors for unresolved zones in section `21`.

### 22.4 Reviewer verification workflow (deterministic)

1. Verify structural completeness:
   run the metric commands in section `22.1` and confirm section inventory includes `8` through `21`.
2. Spot-check traceability:
   pick any row from sections `17`, `20`, or `21`, open referenced source file/line, confirm stated behavior.
3. Verify execution anchors:
   run selected `rch` commands from sections `18`–`21` relevant to the reviewed claim.

### 22.5 Downstream handoff for Pass-B and review beads

Pass-A close-out means this file now supplies the substrate for:
- `bd-2wb.24.10` (unit/e2e/logging crosswalk),
- `bd-2wb.24.12` (EXHAUSTIVE_LEGACY_ANALYSIS Pass B draft),
- `bd-2wb.24.13` (independent red-team contradiction/completeness review),
- `bd-2wb.24.15` (full-agent deep dive Pass A).

## 23. DOC-PASS-14 Full-Agent Deep Dive Pass A (Structure Specialist)

### 23.1 Structure-fidelity scorecard (crate-level architecture)

| Dimension | Assessment | Evidence |
|---|---|---|
| Workspace layering (crate DAG) | strong | root workspace keeps clear layered members (`fr-protocol`/`fr-store` foundations, `fr-command`/`fr-persist` mid-layer, `fr-runtime` assembly, `fr-conformance` harness) with explicit per-crate dependencies |
| Cross-crate coupling direction | strong | runtime is the principal orchestrator (`fr-runtime` depends on command/config/eventloop/protocol/store), conformance depends on runtime + protocol/persist/config/repl |
| Adapter seam clarity | moderate-strong | explicit ecosystem adapter traits exist for async runtime and operator UI surfaces |
| Intra-crate decomposition quality | mixed | several crates are internally monolithic despite clean crate boundaries |
| Packet-surface completeness alignment | mixed-to-weak | packet contract docs are rich, but several packet surfaces remain partial/undefined in implementation |

Source anchors:
- `Cargo.toml:1`
- `crates/fr-runtime/Cargo.toml:6`
- `crates/fr-conformance/Cargo.toml:6`
- `crates/fr-runtime/src/lib.rs:783`

### 23.2 Structural density hotspots (monolith pressure)

| File | LOC snapshot | Function-count signal | Structural risk |
|---|---:|---:|---|
| `crates/fr-conformance/src/lib.rs` | `1760` | `45` functions | harness concerns (fixture loading, live diff, threat checks, log emission) are densely co-located |
| `crates/fr-command/src/lib.rs` | `1565` | `75` functions | command routing + family semantics centralized in single module, increasing edit blast radius |
| `crates/fr-runtime/src/lib.rs` | `1228` | `54` functions | admission/auth/cluster/tls/evidence/eventloop glue converge in one file |
| `crates/fr-config/src/lib.rs` | `900` | high variant density | policy + tls config + parser/validator/apply logic concentrated |

Metric evidence commands used for this pass:
- `for f in crates/*/src/*.rs; do ...; done | sort -nr`
- `rg -n "^\\s*fn " crates/fr-runtime/src/lib.rs | wc -l`
- `rg -n "^\\s*fn " crates/fr-command/src/lib.rs | wc -l`
- `rg -n "^\\s*fn " crates/fr-conformance/src/lib.rs | wc -l`

### 23.3 Decomposition-quality findings (source-anchored)

1. Command dispatch centralization risk:
   `dispatch_argv` currently uses a long linear command-chain and routes many families through one function path.
   Source: `crates/fr-command/src/lib.rs:47`.

2. Runtime orchestration concentration:
   frame parsing, preflight gating, auth handling, cluster command handling, tls policy gating, and evidence recording all co-reside in one module.
   Sources: `crates/fr-runtime/src/lib.rs:285`, `crates/fr-runtime/src/lib.rs:401`, `crates/fr-runtime/src/lib.rs:499`, `crates/fr-runtime/src/lib.rs:587`.

3. Conformance harness concentration:
   fixture execution, threat expectation matching, live-oracle differential execution, and structured log validation are tightly packed in one file.
   Sources: `crates/fr-conformance/src/lib.rs:149`, `crates/fr-conformance/src/lib.rs:672`, `crates/fr-conformance/src/lib.rs:804`.

4. Foundation seam underutilization:
   `fr-expire` is currently a small utility seam (`evaluate_expiry`) and not yet integrated into full expire/evict pipeline orchestration.
   Sources: `crates/fr-expire/src/lib.rs:10`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/implementation_plan.md:32`.

5. Positive seam signal:
   explicit adapter traits in runtime (`AsyncRuntimeAdapter`, `OperatorUiAdapter`) provide clean extension boundaries despite internal runtime concentration.
   Source: `crates/fr-runtime/src/lib.rs:783`.

### 23.4 Priority remediation map (structure-specialist recommendations)

| Priority | Recommendation | Why now |
|---|---|---|
| P1 | Split `fr-runtime` into focused modules: `admission_gate`, `auth_cluster`, `tls_policy`, `evidence` | reduces blast radius on high-risk policy/edit paths and clarifies ownership per subsystem |
| P1 | Replace `fr-command` linear chain with table-driven registry by command family | improves maintainability and parity surface scaling without changing external semantics |
| P2 | Partition `fr-conformance` into runner modules (`fixture_runner`, `live_oracle_runner`, `threat_contract`, `log_contract_bridge`) | keeps verification semantics explicit and reviewable as coverage grows |
| P2 | Elevate `fr-expire` from helper seam to integrated scheduler/policy module | closes structural gap between expire contract artifacts and runtime/store behavior |
| P3 | Isolate packet-specific reason-code families into dedicated namespaces/modules as they land | avoids runtime monolith growth and preserves deterministic auditability |

### 23.5 Structural readiness verdict for Pass A

1. Crate-level architecture is coherent and reviewable.
2. Major risk is not cross-crate entanglement, but large single-file concentration inside key crates (`fr-runtime`, `fr-command`, `fr-conformance`).
3. Documentation now captures these structural realities explicitly, including where packet contracts are ahead of implementation.
4. Pass-A structure-specialist objective is satisfied: subsystem decomposition quality has been audited, risk-ranked, and translated into concrete remediation priorities.

### 23.6 Verification anchors (`rch`)

- `rch exec -- cargo test -p fr-runtime -- --nocapture`
- `rch exec -- cargo test -p fr-command -- --nocapture`
- `rch exec -- cargo test -p fr-conformance -- --nocapture`
- `rch exec -- cargo check --workspace --all-targets`

## 24. DOC-PASS-09 Unit/E2E Test Corpus and Logging Evidence Crosswalk

### 24.1 Crosswalk binding rules

1. Every major behavior claim maps to:
   one concrete unit/property test ID, one E2E/differential fixture or script path, and one structured-log artifact location.
2. High-risk rows must include replay commands for both strict and hardened modes.
3. Coverage gaps are explicitly tracked to follow-up bead IDs; no hidden TODOs.

### 24.2 Behavior-to-verification crosswalk (current)

| Behavior claim | Unit/property evidence (current) | E2E/differential evidence (current) | Structured-log evidence (current) | Strict/Hardened replay bindings | Coverage status |
|---|---|---|---|---|---|
| Event-loop phase ordering and bootstrap invariants (`FR-P2C-001`) | `fr_p2c_001_u001_phase_order_is_deterministic`, `fr_p2c_001_u010_bootstrap_rejects_missing_hooks` (`crates/fr-eventloop/src/lib.rs`) | `core_strings.json` through `run_fixture` (`crates/fr-conformance/src/lib.rs`) plus `fr_p2c_001_e2e_contract_smoke` | `log_contract_v1/FR-P2C-001.golden.jsonl`, optional live JSONL via `live_log_root` | strict: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-eventloop -- --nocapture fr_p2c_001_u001_phase_order_is_deterministic`; hardened: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_001_u005_runtime_blocked_mode_is_bounded` | implemented (`bd-2wb.12.7` closed) |
| Parser/protocol fail-closed semantics (`FR-P2C-002`) | `protocol_invalid_bulk_length_error_string` (`crates/fr-runtime/src/lib.rs`) | `protocol_negative.json`, `fr_p2c_002_e2e_contract_smoke`, plus live differential mode `protocol` in `scripts/run_live_oracle_diff.sh` | `log_contract_v1/FR-P2C-002.golden.jsonl`, bundle report `artifacts/e2e_orchestrator/<run-id>/suites/fr_p2c_002_protocol_negative/report.json` | strict: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture protocol_invalid_bulk_length_error_string`; hardened: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_002_e2e_contract_smoke` | implemented (`bd-2wb.13.7`) |
| Command-dispatch and auth gate ordering (`FR-P2C-003/004`) | `fr_p2c_004_u005_noauth_gate_runs_before_dispatch`, `fr_p2c_007_u001_cluster_subcommand_router_is_deterministic` (`crates/fr-runtime/src/lib.rs`) | `core_errors.json`, `core_strings.json`, plus live command differential mode and packet e2e smokes (`fr_p2c_003_e2e_contract_smoke`, `fr_p2c_004_e2e_contract_smoke`) | `log_contract_v1/FR-P2C-004.golden.jsonl`, live bundle reports and command trace | strict: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u005_noauth_gate_runs_before_dispatch`; hardened: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u004_hello_auth_early_fails_and_success_path_authenticates` | implemented (`bd-2wb.14.7`, `bd-2wb.15.7` closed) |
| Persistence replay ordering and assertions (`FR-P2C-005`) | `fr_p2c_005_e001_replay_fixture_passes_with_schema_contract`, `fr_p2c_005_e003_roundtrip_preserves_aof_record_order_and_payloads`, `fr_p2c_005_e013_adversarial_replay_decode_errors_are_stable`, `fr_p2c_005_fixture_maps_to_packet_family_in_structured_logs` (`crates/fr-conformance/src/lib.rs`) | `persist_replay.json` via `run_replay_fixture`, `fr_p2c_005_e2e_contract_smoke` (`crates/fr-conformance/tests/smoke.rs`), plus packet-005 F-level differential/metamorphic/adversarial tests | `log_contract_v1/FR-P2C-005.golden.jsonl`, replay JSONL under `live_log_root` when enabled | strict: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_005_f_differential_replay_fixture_passes`; hardened: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_005_e2e_contract_smoke` | implemented (`bd-2wb.16.7`) |
| Replication handshake/PSYNC safety reducers (`FR-P2C-006`) | `fr_p2c_006_u003_handshake_requires_ping_first`, `fr_p2c_006_u002_psync_rejects_replid_mismatch` (`crates/fr-repl/src/lib.rs`) | conformance vector tests (`fr_p2c_006_f_*`) + `fr_p2c_006_e2e_contract_smoke` | `log_contract_v1/FR-P2C-006.golden.jsonl`, structured threat/log assertions in conformance | strict: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u003_handshake_requires_ping_first`; hardened: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_psync_adversarial_matrix_prefers_safe_fallbacks` | implemented (`bd-2wb.17.7` closed) |
| Cluster and client-mode boundary behavior (`FR-P2C-007`) | `fr_p2c_007_u001_cluster_subcommand_router_is_deterministic`, `fr_p2c_007_u007_client_cluster_mode_flags_transition_cleanly` | packet-specific journey fixture and smoke coverage (`fr_p2c_007_cluster_journey.json`, `fr_p2c_007_e2e_contract_smoke`) | `log_contract_v1/FR-P2C-007.golden.jsonl` | strict: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_007_u001_cluster_subcommand_router_is_deterministic`; hardened: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_007_u007_client_cluster_mode_flags_transition_cleanly` | implemented (`bd-2wb.18.7` closed) |
| Expire/evict semantics and pressure behavior (`FR-P2C-008`) | packet-tagged unit/property + differential/adversarial coverage (`fr_p2c_008_u*`, `fr_p2c_008_f_*`) with packet optimization parity checks (`fr_p2c_008_dispatch_lookup_*`) | packet journey fixture + smoke coverage (`fr_p2c_008_expire_evict_journey.json`, `fr_p2c_008_e2e_contract_smoke`) | `log_contract_v1/FR-P2C-008.golden.jsonl` | strict: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_`; hardened: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_008_e2e_contract_smoke` | implemented (`bd-2wb.19.7`, `bd-2wb.19.9` closed) |
| TLS/config fail-closed and hardened allowlist policy (`FR-P2C-009`) | `fr_p2c_009_u013_hardened_gate_rejects_non_allowlisted_deviation` (`crates/fr-config/src/lib.rs`), `fr_p2c_009_u013_hardened_non_allowlisted_tls_deviation_is_rejected` (`crates/fr-runtime/src/lib.rs`) | `fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract` (`crates/fr-conformance/src/lib.rs`) + `fr_p2c_009_e2e_contract_smoke` | `log_contract_v1/FR-P2C-009.golden.jsonl`, live bundle reports include `execution_error` surfaces | strict: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u013_strict_mode_rejects_unsafe_tls_config_and_records_event`; hardened: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract` | implemented (`bd-2wb.20.7` closed) |
| Foundation live-oracle differential workflow | `live_oracle_diff` arg/parser tests (`crates/fr-conformance/src/bin/live_oracle_diff.rs`) | `scripts/run_live_oracle_diff.sh` (fixed suite order; deterministic bundle layout) | `suite_status.tsv`, `command_trace.log`, `suites/<suite>/report.json`, `replay_failed.sh`, `live_logs/` | strict+local replay: `./scripts/run_live_oracle_diff.sh --host 127.0.0.1 --port 6379 --run-id <id>`; optional remote runner: `FR_E2E_RUNNER=rch ./scripts/run_live_oracle_diff.sh --host <reachable-host> --port <port> --run-id <id>` | implemented; parity mismatches are explicitly surfaced as failure artifacts, not hidden |

### 24.3 Required forensic log field contract (crosswalk binding)

All crosswalk rows above assume structured log payloads conform to `TEST_LOG_SCHEMA_V1.md`:

- `schema_version`, `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode` (`strict|hardened`), `verification_path` (`unit|property|e2e`), `seed`
- `input_digest`, `output_digest`, `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs` (+ optional `fixture_id`, `env_ref`)

Required evidence anchors:
- golden schema artifacts: `crates/fr-conformance/fixtures/log_contract_v1/*.golden.jsonl`
- live execution artifacts: `artifacts/e2e_orchestrator/<run-id>/...`

### 24.4 Coverage-gap backlog bindings (explicit)

| Gap ID | Missing surface | Follow-up bead | Dependency target / unblock | CI-gate tie |
|---|---|---|---|---|
| CV-001 | Packet-specific E2E suite for `FR-P2C-001` | `bd-2wb.12.7` | packet-001 closure chain | implemented via closed `bd-2wb.10` |
| CV-002 | Packet-specific E2E suite for `FR-P2C-002` | `bd-2wb.13.7` | packet-002 closure chain | implemented (`bd-2wb.13.7`) |
| CV-003 | Packet-specific E2E suite for `FR-P2C-003` | `bd-2wb.14.7` | packet-003 closure chain | implemented via closed `bd-2wb.10` |
| CV-004 | Packet-specific E2E suite for `FR-P2C-004` | `bd-2wb.15.7` | packet-004 closure chain | implemented via closed `bd-2wb.10` |
| CV-005 | Packet-specific E2E suite for `FR-P2C-005` | `bd-2wb.16.7` | packet-005 closure chain | implemented (`bd-2wb.16.7`) |
| CV-006 | Packet-specific E2E suite for `FR-P2C-006` | `bd-2wb.17.7` | packet-006 closure chain | implemented via closed `bd-2wb.10` |
| CV-007 | Packet-specific E2E suite for `FR-P2C-007` | `bd-2wb.18.7` | packet-007 closure chain | implemented via closed `bd-2wb.10` |
| CV-008 | Packet-specific E2E suite for `FR-P2C-008` | `bd-2wb.19.7` | packet-008 closure chain | implemented (`bd-2wb.19.7` closed; CI path via closed `bd-2wb.10`) |
| CV-009 | Packet-specific E2E suite for `FR-P2C-009` | `bd-2wb.20.7` | packet-009 closure chain | implemented via closed `bd-2wb.10` |
| CV-010 | Reliability/flake budget policy for corpus | `bd-2wb.23` | unblocks packet test gates | implemented (closed `bd-2wb.23` + `bd-2wb.10`) |
| CV-011 | Operator-facing failure forensics index/UX | `bd-2wb.22` | post-failure triage flow | implemented (closed `bd-2wb.22` + `bd-2wb.10`) |

### 24.5 Verification command set for this crosswalk (`rch` + orchestrator)

- `rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture`
- `rch exec -- cargo test -p fr-conformance --bin live_oracle_diff -- --nocapture`
- `rch exec -- cargo check -p fr-conformance --all-targets`
- `./scripts/run_live_oracle_diff.sh --host 127.0.0.1 --port 6379 --run-id <run-id>`

## 25. DOC-PASS-12 Cross-Document Red-Team Contradiction Reconciliation

### 25.1 Cross-document contradiction matrix (structure vs behavior claims)

| ID | Claim surface | Adversarial finding | Evidence anchors | Resolution state |
|---|---|---|---|---|
| `XDOC-001` | Architecture chain implies fully connected runtime path through persistence and replication | Current runtime execution path is store-centric; persistence/replication kernels exist but are not integrated into `Runtime::execute_frame` flow | `crates/fr-runtime/src/lib.rs`, `crates/fr-persist/src/lib.rs`, `crates/fr-repl/src/lib.rs`, `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `20.2` (`RT-001`) | bounded unresolved gap; tracked by `bd-2wb.16`, `bd-2wb.16.7`, `bd-2wb.17`, `bd-2wb.17.7` |
| `XDOC-002` | Expire crate role can be read as complete runtime TTL orchestration | `fr-expire` currently provides a reusable decision helper while runtime behavior is implemented via store lazy-expiry path | `crates/fr-expire/src/lib.rs`, `crates/fr-store/src/lib.rs`, `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `20.2` (`RT-002`) | bounded architecture caveat; packet-008 evidence chain is closed (`bd-2wb.19*`), while active-expire/eviction orchestration remains explicitly out of implemented scope |
| `XDOC-003` | Cluster packet contract includes deferred `clusterBeforeSleep` actions that may be interpreted as current behavior | Runtime currently exposes deterministic scaffold (`CLUSTER HELP` plus client mode toggles); deferred tasks remain contractual backlog | `crates/fr-runtime/src/lib.rs`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md`, `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `20.2` (`RT-003`) | bounded unresolved gap; tracked by `bd-2wb.18`, `bd-2wb.18.7` |
| `XDOC-004` | RaptorQ-everywhere doctrine can be read as globally complete in current repo state | Foundation sidecar + decode-proof pipeline and CI gate ingestion are now wired; remaining scope is incremental artifact-surface expansion | `scripts/run_raptorq_artifact_gate.sh`, `.github/workflows/live-conformance-gates.yml`, `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `20.2` (`RT-004`) | foundation resolved (closed `bd-2wb.9`, `bd-2wb.10`); continue scaling coverage with new artifact classes |

### 25.2 Completeness challenge results

Independent challenge questions and outcomes:
1. Are any packet-level closure claims present without explicit unit/e2e/log anchors?
Answer: no; section `24` crosswalk rows include evidence anchors and explicit closure status for packet E2E follow-up beads (all packet `.7` beads are closed).
2. Are strict/hardened boundaries stated for high-risk behavior classes?
Answer: yes, but unresolved implementation zones remain explicitly bounded by open beads; no “silent parity complete” language is allowed for those zones.
3. Are replay commands and forensic fields concrete for high-risk claims?
Answer: yes; command anchors and forensic field contract are documented in section `24.2` and `24.3`, with carry-forward validation in `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `20.7`.

### 25.3 Pass-B-to-Pass-C handoff constraints for final integration

Before `bd-2wb.24.14` sign-off, the following reconciliation checks remain mandatory:
1. each `XDOC-*` row above must either transition to resolved-by-evidence or remain explicitly blocked with unchanged risk bounds;
2. packet E2E closure beads (`bd-2wb.12.7`..`bd-2wb.20.7`) must stay visible in crosswalk tables with explicit closure status (all packet `.7` beads are now closed);
3. CI gate topology now ingests contradiction-bounded artifact expectations (`suite_status.tsv`, per-suite `report.json`, replay pointers, structured log contract fields); keep schema and replay pointers stable as packets close;
4. no section may claim “parity complete” for persistence, replication, cluster deferred tasks, or full eviction orchestration until corresponding implementation and verification beads are closed.

## 26. DOC-PASS-15 Behavior-Specialist Deep-Dive Crosswalk

### 26.1 Behavioral interpretation scorecard

| Behavior surface | Interpretation status | Primary evidence anchors | Remaining bound |
|---|---|---|---|
| Parser/admission fail-closed semantics | clarified and validated | `crates/fr-runtime/src/lib.rs` (`protocol_invalid_bulk_length_error_string`), `crates/fr-conformance/src/lib.rs` (`conformance_protocol_fixture_passes`) | none beyond packet E2E depth work |
| Auth/HELLO ordering semantics | clarified and validated for implemented path | `crates/fr-runtime/src/lib.rs` (`fr_p2c_004_u004_*`, `fr_p2c_004_u005_*`) | packet-specific E2E closure (`bd-2wb.15.7`) |
| Cluster implemented subset semantics | clarified as subset-only | `crates/fr-runtime/src/lib.rs` (`fr_p2c_007_u001_*`, `fr_p2c_007_u007_*`), `FR-P2C-007` contract table deferred row | deferred-task implementation and packet E2E (`bd-2wb.18.7`) |
| Replication reducer semantics | validated at kernel level | `crates/fr-repl/src/lib.rs` (`fr_p2c_006_u003_*`, `fr_p2c_006_u002_*`), `crates/fr-conformance/src/lib.rs` (`fr_p2c_006_f_*`) | runtime integration + packet E2E (`bd-2wb.17.7`) |
| Expire/evict semantics | clarified as current lazy-expiry scope | `crates/fr-store/src/lib.rs` (`drop_if_expired`, `expire_*` tests), `crates/fr-expire/src/lib.rs` helper kernel | packet E2E chain is closed; remaining caveat is active-expire/eviction orchestration beyond current lazy semantics |
| TLS/config strict-hardened boundary | validated and explicitly bounded | `crates/fr-runtime/src/lib.rs` (`fr_p2c_009_u013_*`), `crates/fr-config/src/lib.rs` (`fr_p2c_009_u013_*`), `crates/fr-conformance/src/lib.rs` (`fr_p2c_009_e013_*`) | packet E2E depth (`bd-2wb.20.7`) |

### 26.2 Cross-document carry-forward bindings

Behavior-specialist outputs are canonicalized in:
- `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `21.2` (invariant validation matrix),
- `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `21.4` (strict/hardened replay matrix),
- `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `21.6` (expected-loss behavior drift model),
- `EXHAUSTIVE_LEGACY_ANALYSIS.md` section `21.9` (behavior gap ledger).

This structure file carries the same constraints through:
- section `24` (unit/e2e/log crosswalk),
- section `25` (contradiction reconciliation),
- this section `26` (behavior-interpretation closure state).

### 26.3 Pass-B deep-dive closure criteria for `bd-2wb.24.16`

1. Behavior ambiguities are either clarified with source anchors or explicitly bounded by open beads.
2. High-risk behavior claims include strict/hardened replay commands and forensic-field expectations.
3. Invariant narratives are mapped to unit/e2e/log evidence and corrected where over-claims were possible.
4. Remaining behavior uncertainty is dependency-tracked and cannot be interpreted as parity closure.

## 27. DOC-PASS-16 Risk/Perf/Test Specialist Crosswalk

### 27.1 Lane-level readiness summary

| Lane | Current status | Evidence substrate | Blocker beads |
|---|---|---|---|
| Risk taxonomy and mitigation | foundation-ready with packet closure evidence complete | strict/hardened threat policy types, runtime evidence hooks, CI differential/forensics gates | no remaining packet-chain blocker; maintain architecture-bound caveats (`RT-001..RT-003`) |
| Performance proof loop | foundation-ready with packet optimization closure complete | baseline artifacts + optimization evidence pack in `artifacts/optimization/phase2c-gate/round_dir_scan_mask/` + CI optimization gate wiring | packet-008 optimization/final evidence chain closed (`bd-2wb.19.8`, `bd-2wb.19.9`) |
| Unit/property depth | mostly ready at foundation level | packet-tagged unit tests in runtime/repl/config/store | packet `*.5` and `*.6` closure chains |
| Deterministic E2E depth | foundation-ready with full packet closure | orchestrator + live differential bundle schema + packet `.7` closures across `FR-P2C-001..FR-P2C-009` | none at packet `.7` layer (all closed) |
| Structured log forensics | schema-ready and CI-integrated | log contract (`TEST_LOG_SCHEMA_V1.md`, `log_contract.rs`) + failure-forensics CI index + golden packet logs | none at packet-depth closure layer |
| RaptorQ evidence durability | foundation-ready and CI-integrated | sidecar/decode proof artifacts + CI RaptorQ artifact gate report | incremental artifact-surface expansion (no open foundation blocker) |

### 27.2 Gate-state interpretation for final integration

`bd-2wb.10` (CI gate topology) is closed and now acts as the active integration backbone.
Packet-008 closure chain is now complete:
- `bd-2wb.19.6` differential/adversarial closure (closed),
- `bd-2wb.19.7` packet E2E journey closure (closed),
- `bd-2wb.19.8` optimization/isomorphism closure (closed),
- `bd-2wb.19.9` final evidence-pack closure (closed).

Interpretation rule:
- with packet-008 blockers closed, docs may assert packet-chain gate completeness; keep architecture-bound caveats (`RT-001..RT-003`) explicit and unchanged.

### 27.3 Pass-C closure criteria for `bd-2wb.24.17`

1. Risk/perf/test narratives are explicitly mapped to unit/e2e/log evidence and blocker beads.
2. Missing mappings are bounded and dependency-tracked, not implicit.
3. Go/no-go semantics for final integrated sign-off are explicit and fail-closed.
4. Replay command set for high-risk checks is concrete and reproducible with `rch`.
