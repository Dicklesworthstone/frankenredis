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
