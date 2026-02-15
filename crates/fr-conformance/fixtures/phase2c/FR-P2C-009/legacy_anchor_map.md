# FR-P2C-009 Legacy Anchor Map

Packet: `FR-P2C-009`  
Subsystem: TLS/config boundary  
Target crates: `crates/fr-config`, `crates/fr-runtime`, `crates/fr-eventloop`, `crates/fr-conformance`  
Prepared by: `PeachKnoll`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact extracts line-anchored TLS/config behavior contracts from legacy
Redis and maps them to current FrankenRedis coverage, including TLS context
configuration, handshake/event-loop I/O semantics, runtime config mutation
behavior, and rewrite/persistence guarantees for downstream packet validation.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-009-A01` | `legacy_redis_code/redis/src/tls.c:48-80` | `parseProtocolsConfig` validates `tls-protocols` tokens and falls back to deterministic protocol defaults when needed. | Missing TLS protocol parser surface in Rust config.
| `FR-P2C-009-A02` | `legacy_redis_code/redis/src/tls.c:124-165` | `tlsInit`/`tlsCleanup` provide deterministic OpenSSL lifecycle initialization/teardown boundaries. | Missing TLS runtime lifecycle bootstrap.
| `FR-P2C-009-A03` | `legacy_redis_code/redis/src/tls.c:181-254` | `createSSLContext` enforces cert/key/CA/cipher correctness and aborts on invalid context build. | Missing SSL context constructor and validation pipeline.
| `FR-P2C-009-A04` | `legacy_redis_code/redis/src/tls.c:267-393` | `tlsConfigure` atomically applies TLS settings (session cache, DH, client auth policy) and swaps contexts only on success. | Missing atomic TLS reconfiguration path.
| `FR-P2C-009-A05` | `legacy_redis_code/redis/src/server.h:1843-1861` | `redisTLSContextConfig` defines authoritative TLS config schema and field ownership. | Missing equivalent config struct/schema in `fr-config`.
| `FR-P2C-009-A06` | `legacy_redis_code/redis/src/server.h:2474-2479` | Server-level TLS flags (`tls_cluster`, `tls_replication`, `tls_auth_clients`) drive policy at runtime boundaries. | Missing TLS policy flags in Rust runtime/config.
| `FR-P2C-009-A07` | `legacy_redis_code/redis/src/server.c:3074-3140` | Listener initialization registers TLS connection type and enables TLS listeners only when TLS policy/ports demand it. | Missing TLS listener registration path.
| `FR-P2C-009-A08` | `legacy_redis_code/redis/src/tls.c:420-470` | `createTLSConnection`/`connCreateTLS` wrap sockets with deterministic TLS connection state. | Missing TLS connection abstraction in Rust.
| `FR-P2C-009-A09` | `legacy_redis_code/redis/src/tls.c:450-513` | `connCreateAcceptedTLS` maps `tls-auth-clients` policy to `SSL_set_verify` for accepted sessions. | Missing accepted-session client-auth policy hook.
| `FR-P2C-009-A10` | `legacy_redis_code/redis/src/tls.c:505-575` | `handleSSLReturnCode` + `updateStateAfterSSLIO` map SSL I/O outcomes to deterministic `WANT`/error/closed states. | Missing TLS I/O error-state normalizer.
| `FR-P2C-009-A11` | `legacy_redis_code/redis/src/tls.c:578-640` | SSL event registration updates AE readable/writable interest based on pending TLS wants/data. | Missing TLS-aware event-loop subscription logic.
| `FR-P2C-009-A12` | `legacy_redis_code/redis/src/tls.c:656-685` | `tlsGetPeerUsername` extracts configured cert field for user identity mapping. | Missing certificate-field identity extraction contract.
| `FR-P2C-009-A13` | `legacy_redis_code/redis/src/tls.c:688-860` | TLS event handler drives deterministic CONNECTING/ACCEPTING/CONNECTED transitions with re-armed wants. | Missing TLS handshake state machine.
| `FR-P2C-009-A14` | `legacy_redis_code/redis/src/tls.c:860-910` | Accept/connect path (`SSL_accept`/`SSL_connect`) uses non-blocking WANT progression and SNI behavior. | Missing async TLS connect/accept orchestration.
| `FR-P2C-009-A15` | `legacy_redis_code/redis/src/tls.c:960-1032` | TLS read/write path enforces bounded writes and deterministic errno/EAGAIN semantics. | Missing TLS read/write bounded I/O semantics.
| `FR-P2C-009-A16` | `legacy_redis_code/redis/src/config.c:3231-3232` | `cluster-announce-tls-port` config keeps cluster announcements aligned with TLS listener surface. | Missing TLS-specific cluster announce config entry.
| `FR-P2C-009-A17` | `legacy_redis_code/redis/src/config.c:3253` | `max-new-tls-connections-per-cycle` throttles TLS accepts per cycle deterministically. | Missing per-cycle TLS accept throttle config.
| `FR-P2C-009-A18` | `legacy_redis_code/redis/src/config.c:3304-3324` | TLS directives are declared in config table with MODIFIABLE/SENSITIVE traits and routed to TLS apply hooks. | Missing table-driven TLS directive registry.
| `FR-P2C-009-A19` | `legacy_redis_code/redis/src/config.c:3332` | `bind` apply path couples TCP/TLS listener updates to preserve address consistency. | Missing coupled TCP/TLS bind apply path.
| `FR-P2C-009-A20` | `legacy_redis_code/redis/src/config.c:2632-2670` | `applyBind` keeps listener transitions fail-safe: TLS bind failure rolls back paired listener updates. | Missing atomic listener rebind rollback logic.
| `FR-P2C-009-A21` | `legacy_redis_code/redis/src/config.c:2701-2745` | `applyTlsCfg`/`applyTLSPort` force runtime TLS context reconfiguration and listener rebinding after config mutations. | Missing runtime `CONFIG SET` TLS reconfigure flow.
| `FR-P2C-009-A22` | `legacy_redis_code/redis/src/connection.h:420-446` | `connTypeConfigure` is the generic connection-type config hook reused by TLS startup and runtime updates. | Missing configurable connection-type interface for TLS.
| `FR-P2C-009-A23` | `legacy_redis_code/redis/src/config.c:1758-1801` | `rewriteConfig` atomically rewrites config entries and preserves TLS directives through persistence cycles. | Missing config rewrite persistence contract for TLS settings.
| `FR-P2C-009-A24` | `crates/fr-config/src/lib.rs:1-112` | Rust `fr-config` currently contains mode/threat/policy logic only; no TLS schema/parse/apply primitives exist. | Present as policy baseline only; TLS config surface absent.
| `FR-P2C-009-A25` | `crates/fr-runtime/src/lib.rs:1-220`, `crates/fr-eventloop/src/lib.rs:1-56` | Rust runtime/eventloop currently handle protocol gating and generic tick budgeting, with no TLS connection type, handshake loop, or TLS listener orchestration. | Present as non-TLS baseline only.

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-009-B01` | Normal | TLS protocols config parsed | Unsupported/invalid protocol tokens are rejected or normalized deterministically. | `FR-P2C-009-U001` | `FR-P2C-009-E001` | `tlscfg.protocols_parse_contract_violation` |
| `FR-P2C-009-B02` | Normal | TLS context initialization | Cert/key/CA/cipher preconditions gate context creation; partial context apply is forbidden. | `FR-P2C-009-U002` | `FR-P2C-009-E002` | `tlscfg.context_build_contract_violation` |
| `FR-P2C-009-B03` | Normal | TLS runtime reconfiguration | Context pointers swap atomically only after full validation and setup succeeds. | `FR-P2C-009-U003` | `FR-P2C-009-E003` | `tlscfg.atomic_reconfigure_violation` |
| `FR-P2C-009-B04` | Normal | Accepted TLS connection created | `tls-auth-clients` policy maps to deterministic peer-verify behavior. | `FR-P2C-009-U004` | `FR-P2C-009-E004` | `tls.handshake_verify_policy_violation` |
| `FR-P2C-009-B05` | Edge | TLS I/O returns WANT/ERROR paths | Event-loop interest and connection state transitions remain deterministic and non-blocking. | `FR-P2C-009-U005` | `FR-P2C-009-E005` | `tls.io_state_transition_violation` |
| `FR-P2C-009-B06` | Normal | TLS peer identity extraction requested | Configured certificate field extraction contract is deterministic and policy-compliant. | `FR-P2C-009-U006` | `FR-P2C-009-E006` | `tls.peer_identity_contract_violation` |
| `FR-P2C-009-B07` | Normal | Listener bootstrap under TLS mode | TLS listeners are registered/enabled only when policy and port settings require them. | `FR-P2C-009-U007` | `FR-P2C-009-E007` | `tls.listener_bootstrap_contract_violation` |
| `FR-P2C-009-B08` | Normal | `CONFIG SET` updates TLS directives | Runtime apply path reconfigures TLS context and bound listeners deterministically. | `FR-P2C-009-U008` | `FR-P2C-009-E008` | `tlscfg.runtime_apply_contract_violation` |
| `FR-P2C-009-B09` | Edge | `bind` mutation affects TCP/TLS listeners | Paired listener transitions are atomic; failed TLS rebind does not leave mixed listener state. | `FR-P2C-009-U009` | `FR-P2C-009-E009` | `tlscfg.bind_atomicity_violation` |
| `FR-P2C-009-B10` | Normal | Config rewrite persists TLS options | Rewrite output preserves TLS directives deterministically for restart parity. | `FR-P2C-009-U010` | `FR-P2C-009-E010` | `tlscfg.rewrite_persistence_violation` |
| `FR-P2C-009-B11` | Adversarial | Connection bursts hit per-cycle TLS limits | `max-new-tls-connections-per-cycle` enforcement keeps accept rate bounded. | `FR-P2C-009-U011` | `FR-P2C-009-E011` | `tls.accept_rate_limit_violation` |
| `FR-P2C-009-B12` | Adversarial | Hardened mode sees non-allowlisted TLS/config deviation | Non-allowlisted behavior changes are rejected and strict-equivalent contract is preserved. | `FR-P2C-009-U012` | `FR-P2C-009-E012` | `tlscfg.hardened_nonallowlisted_rejected` |

## High-risk traceability and structured-log contract

For all `FR-P2C-009-U*` and `FR-P2C-009-E*` rows, logs must include:

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

Suggested suite IDs:

- Unit/property: `fr_tls_config_phase2c_packet_009`
- E2E/integration: `fr_runtime_phase2c_packet_009`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-009-CLAIM-01` |
| `evidence_id` | `FR-P2C-009-EVID-LEGACY-TLSCFG-001` |
| Hotspot evidence | `A04`, `A10`, `A21` (atomic TLS context configure, SSL I/O state machine, runtime apply/rebind path) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-SEC-11`, `AG-NET-06` |
| Baseline comparator | Legacy Redis TLS/config path (`tls.c`, `config.c`, `server.c`, `connection.h`) |
| EV score | `3.0` |
| Priority tier | `S` |
| Adoption wedge | Land config schema+validation and atomic TLS context apply first, then handshake/event-loop integration and runtime rewrite parity |
| Budgeted mode defaults | Strict: `FailClosed`; Hardened: bounded defenses on explicit allowlist only |
| Deterministic exhaustion behavior | Hardened budget exhaustion forces strict-equivalent fail-closed and emits `tlscfg.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-config -- --nocapture FR_P2C_009`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_009` |

## Expected-loss decision model

States:

- `S0`: contract-preserving TLS/config behavior
- `S1`: recoverable bounded condition (allowlisted)
- `S2`: unsafe TLS/config divergence condition

Actions:

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense with evidence emission
- `A2`: fail closed and reject transition

Loss matrix (lower is better):

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)` rises with bounded parser/diagnostic anomalies without listener/context drift.
- `P(S2|e)` rises with TLS context apply failures, handshake-state drift, or rewrite/apply contract mismatches.

Calibration and fallback policy:

- Calibration metric: false-negative rate on adversarial TLS/config suite `< 1%`.
- Fallback trigger: calibration breach in two consecutive windows or `P(S2|e) >= 0.30`.
- Trigger behavior: disable hardened deviations for packet scope and enforce strict fail-closed behavior.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever (downstream execution target):

- `LEV-009-01`: deterministic TLS handshake/config decision memo keyed by `(policy_epoch, tls_cfg_digest, listener_state_mask)` with strict invalidation on config mutation and listener rebind events.

Required loop artifacts and paths:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-009/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-009/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-009/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-009/isomorphism_report.md`

Replay commands (strict/hardened):

- `rch exec -- cargo test -p fr-config -- --nocapture FR_P2C_009_STRICT`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_009_HARDENED`

## Reproducibility and provenance pack references

Required pack (to be produced and linked by downstream packet beads):

- `artifacts/phase2c/FR-P2C-009/env.json`
- `artifacts/phase2c/FR-P2C-009/manifest.json`
- `artifacts/phase2c/FR-P2C-009/repro.lock`
- `artifacts/phase2c/FR-P2C-009/LEGAL.md` (mandatory if IP/provenance risk is detected)

## Confidence notes

- High confidence for TLS context/config lifecycle anchor extraction from `tls.c` and config apply/rewrite anchors from `config.c`.
- Medium confidence for exact handshake-event loop parity edges until Rust TLS connection type exists.
- High confidence that current Rust coverage is policy-only (`fr-config`) and does not yet implement TLS/config runtime parity paths.
