# EXHAUSTIVE_LEGACY_ANALYSIS.md — FrankenRedis

Date: 2026-02-13  
Method stack: `$porting-to-rust` Phase-2 Deep Extraction + `$alien-artifact-coding` + `$extreme-software-optimization` + RaptorQ durability + frankenlibc/frankenfs strict/hardened doctrine.

## 0. Mission and Completion Criteria

This document defines exhaustive legacy extraction for FrankenRedis. Phase-2 is complete only when each subsystem in the full parity program has:
1. explicit invariants,
2. explicit crate ownership,
3. explicit oracle families,
4. explicit strict/hardened policy behavior,
5. explicit performance and durability gates.

## 1. Source-of-Truth Crosswalk

Legacy corpus:
- `/data/projects/frankenredis/legacy_redis_code/redis`
- Upstream oracle: `redis/redis`

Project contracts:
- `/data/projects/frankenredis/COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md`
- `/data/projects/frankenredis/EXISTING_REDIS_STRUCTURE.md`
- `/data/projects/frankenredis/PLAN_TO_PORT_REDIS_TO_RUST.md`
- `/data/projects/frankenredis/PROPOSED_ARCHITECTURE.md`
- `/data/projects/frankenredis/FEATURE_PARITY.md`

Important specification gap:
- the comprehensive spec currently defines sections `0-13` then jumps to `21`; missing sections for crate contracts/conformance matrix/threat matrix/perf budgets/CI/RaptorQ envelope must be backfilled.

## 2. Quantitative Legacy Inventory (Measured)

- Total files: `1772`
- Native: `c=449`, `h=297`, `cpp=8`
- Python/tcl/scripts: `py=41`, `tcl=222`, `sh=69`
- Test-like files: `540`

High-density zones:
- `src/commands` (421 files)
- `src/modules` (10)
- test corpus under `tests/` and `runtest`

## 3. Subsystem Extraction Matrix (Legacy -> Rust)

| Legacy locus | Non-negotiable behavior to preserve | Target crates | Primary oracles | Phase-2 extraction deliverables |
|---|---|---|---|---|
| `src/ae.c`, `ae_*.c`, `anet.c`, `connection.c` | readiness-driven event loop semantics | `fr-eventloop` | `tests/unit/*`, integration network fixtures | event state machine and wakeup ordering ledger |
| `src/resp_parser.c`, `call_reply.c` | RESP frame parsing and reply encoding | `fr-protocol` | `tests/vectorset/*`, protocol unit suites | parser DFA + error/recovery map |
| `src/commands.c`, `commands.def`, `server.c`, `acl.c` | command registry, dispatch flags, ACL behavior | `fr-command` | `tests/unit/*`, ACL/config fixtures | command flag matrix + dispatch contract |
| `src/db.c`, `object.c`, `dict.c`, `rax.c`, `quicklist.c` | key/value and data-structure semantics | `fr-store` | `tests/unit/*` | data-structure invariant ledger |
| `src/expire.c`, `evict.c` | TTL/eviction behavior and ordering | `fr-expire` | unit/integration ttl suites | expiration decision table |
| `src/rdb.c`, `aof.c`, `rio.c`, `bio.c` | persistence format and replay semantics | `fr-persist` | persistence integration suites | replay/state transition matrix |
| `src/replication.c`, `cluster.c` | replication offsets and cluster slot semantics | `fr-repl` | `tests/integration/*`, `tests/cluster/*` | replication state-machine ledger |
| `src/config.c`, `tls.c`, `module.c` | config compatibility and secure transport behavior | `fr-config` | config/tls/module fixtures | config option compatibility map |

## 4. Alien-Artifact Invariant Ledger (Formal Obligations)

- `FR-I1` RESP completeness: command argv is created only from fully parsed frames.
- `FR-I2` Event ordering safety: read/write readiness semantics are deterministic under scheduler rules for parity-target workloads.
- `FR-I3` Command authorization integrity: ACL and command flags jointly enforce full access-rule parity.
- `FR-I4` Persistence replay correctness: replayed state is observationally equivalent for parity-target command surface.
- `FR-I5` Replication offset consistency: replication never silently regresses committed offset semantics for parity-target paths.

Required proof artifacts per implemented slice:
1. invariant statement,
2. executable witness fixtures,
3. counterexample archive,
4. remediation proof.

## 5. Native/OS Boundary Register

| Boundary | Files | Risk | Mandatory mitigation |
|---|---|---|---|
| event backend OS integration | `ae_epoll.c`, `ae_kqueue.c`, `ae_select.c` | critical | backend-specific readiness fixtures |
| network/socket boundary | `anet.c`, `socket.c` | high | malformed frame and disconnect stress corpus |
| persistence/fs boundary | `rdb.c`, `aof.c`, `rio.c`, `bio.c` | critical | replay + corruption recovery differential fixtures |
| tls/security boundary | `tls.c`, `acl.c` | high | auth+tls misconfiguration corpus |

## 6. Compatibility and Security Doctrine (Mode-Split)

Decision law (runtime):
`mode + protocol_contract + risk_score + budget -> allow | full_validate | fail_closed`

| Threat | Strict mode | Hardened mode | Required ledger artifact |
|---|---|---|---|
| malformed RESP payload | fail-closed | fail-closed with bounded diagnostics | parser incident ledger |
| ACL confusion | fail unauthorized path | fail unauthorized path + additional auditing | auth decision ledger |
| command abuse payload (size/explosion) | execute documented behavior | admission guard with bounded limits | admission report |
| persistence corruption | fail and mark incident | attempt bounded recovery then audit | replay/recovery ledger |
| unknown incompatible config/metadata | fail-closed | fail-closed | compatibility drift report |

## 7. Conformance Program (Exhaustive First Wave)

### 7.1 Fixture families

1. RESP parser and reply fixtures (`tests/vectorset`)
2. Command core fixtures (`tests/unit`)
3. Integration command+persistence fixtures (`tests/integration`)
4. Replication/cluster fixtures (`tests/cluster`)
5. ACL/config fixtures
6. Module boundary fixtures (`tests/modules`)

### 7.2 Differential harness outputs (`fr-conformance`)

Each run emits:
- machine-readable parity report,
- mismatch taxonomy,
- minimized repro bundle,
- strict/hardened divergence report.

Release gate rule: critical-family drift => hard fail.

## 8. Extreme Optimization Program

Primary hotspots:
- RESP parse/encode loop
- command dispatch fast path
- keyspace lookup and TTL checks
- persistence serialization and replay loops

Current governance state:
- comprehensive spec lacks explicit numeric budgets (sections 14-20 absent).

Provisional Phase-2 budgets (must be ratified):
- parser+dispatch p95 regression <= +8%
- persistence replay p95 regression <= +10%
- p99 regression <= +10%, RSS regression <= +10%

Optimization governance:
1. baseline,
2. profile,
3. one lever,
4. conformance proof,
5. budget gate,
6. evidence commit.

## 9. RaptorQ-Everywhere Artifact Contract

Durable artifacts requiring RaptorQ sidecars:
- conformance fixture bundles,
- replay baselines,
- replication and compatibility ledgers.

Required envelope fields:
- source hash,
- symbol manifest,
- scrub status,
- decode proof chain.

## 10. Phase-2 Execution Backlog (Concrete)

1. Extract eventloop readiness state transitions.
2. Extract RESP parser DFA and framing error surfaces.
3. Extract command flag and ACL compatibility matrix.
4. Extract keyspace structure and mutation invariants.
5. Extract TTL/eviction decision semantics.
6. Extract RDB/AOF replay transition semantics.
7. Extract replication and cluster slot state transitions.
8. Extract config option compatibility and TLS behavior.
9. Build first differential fixture corpus for items 1-8.
10. Implement mismatch taxonomy in `fr-conformance`.
11. Add strict/hardened divergence reporting.
12. Attach RaptorQ sidecar generation and decode-proof validation.
13. Ratify section-14-20 budgets/gates against first benchmark and conformance runs.

Definition of done for Phase-2:
- each section-3 row has extraction artifacts,
- all six fixture families runnable,
- governance sections 14-20 empirically ratified and mapped to harness outputs.

## 11. Residual Gaps and Risks

- sections 14-20 now exist; top release risk is budget miscalibration before first full benchmark cycle.
- `PROPOSED_ARCHITECTURE.md` crate map formatting contains literal `\n`; normalize before automation.
- replication/persistence paths are highest risk for silent behavioral drift without broad differential corpus.

## 12. Deep-Pass Hotspot Inventory (Measured)

Measured from `/data/projects/frankenredis/legacy_redis_code/redis/src`:
- file count: `637`
- monolithic C core with behavior concentrated in `server.c`, `networking.c`, `rdb.c`, `replication.c`, `cluster*.c`

Top source hotspots by line count (first-wave extraction anchors):
1. `module.c` (`15551`)
2. `redis-cli.c` (`11143`)
3. `server.c` (`8030`)
4. `cluster_legacy.c` (`6581`)
5. `t_stream.c` (`5796`)
6. `networking.c` (`5750`)
7. `replication.c` (`5391`)
8. `rdb.c` (`4484`)

Interpretation:
- command/eventloop/protocol/persistence/replication boundaries are tightly coupled,
- extraction must prioritize state-machine explicitness over module naming,
- ACL/config and TLS edges need hardened-policy clarity from day one.

## 13. Phase-2C Extraction Payload Contract (Per Ticket)

Each `FR-P2C-*` ticket MUST produce:
1. state-machine inventory (states/transitions/guards),
2. parser/protocol rule ledger (including framing errors),
3. command/ACL behavior tables,
4. persistence/replication transition contracts,
5. error + diagnostics contract map,
6. strict/hardened split policy,
7. exclusion ledger,
8. fixture mapping manifest,
9. optimization candidate + isomorphism risk note,
10. RaptorQ artifact declaration.

Artifact location (normative):
- `artifacts/phase2c/FR-P2C-00X/legacy_anchor_map.md`
- `artifacts/phase2c/FR-P2C-00X/contract_table.md`
- `artifacts/phase2c/FR-P2C-00X/fixture_manifest.json`
- `artifacts/phase2c/FR-P2C-00X/parity_gate.yaml`
- `artifacts/phase2c/FR-P2C-00X/risk_note.md`

## 14. Strict/Hardened Compatibility Drift Budgets

Packet acceptance budgets:
- strict critical drift budget: `0`
- strict non-critical drift budget: `<= 0.10%`
- hardened divergence budget: `<= 1.00%` and only allowlisted defensive classes
- unknown protocol/config/metadata behavior: fail-closed

Per-packet report fields:
- `strict_parity`,
- `hardened_parity`,
- `protocol_drift_summary`,
- `replication_state_drift_summary`,
- `compatibility_drift_hash`.

## 15. Extreme-Software-Optimization Execution Law

Mandatory loop:
1. baseline,
2. profile,
3. one lever,
4. conformance + invariant replay,
5. re-baseline.

Primary sentinel workloads:
- RESP parsing under mixed payload sizes (`FR-P2C-002`),
- command dispatch with ACL checks (`FR-P2C-003`, `FR-P2C-004`),
- TTL/eviction pressure (`FR-P2C-008`),
- replication catch-up traces (`FR-P2C-006`).

Optimization scoring gate:
`score = (impact * confidence) / effort`, merge only if `score >= 2.0`.

## 16. RaptorQ Evidence Topology and Recovery Drills

Durable artifacts requiring sidecars:
- parity reports,
- protocol mismatch corpora,
- persistence/replication ledgers,
- benchmark baselines,
- strict/hardened decision logs.

Naming convention:
- payload: `packet_<id>_<artifact>.json`
- sidecar: `packet_<id>_<artifact>.raptorq.json`
- proof: `packet_<id>_<artifact>.decode_proof.json`

Any decode-proof failure blocks packet promotion.

## 17. Phase-2C Exit Checklist (Operational)

Phase-2C is complete only when:
1. `FR-P2C-001..009` artifacts exist and validate.
2. All packets have strict and hardened fixture coverage.
3. Drift budgets from section 14 are met.
4. High-risk packets include optimization proof artifacts.
5. RaptorQ sidecars + decode proofs are scrub-clean.
6. Governance backfill tasks are tied to packet evidence.

## 18. DOC-PASS-00 Baseline Gap Matrix + Quantitative Expansion Targets

### 18.1 Scoring rubric (auditable)

- Coverage score (`0..5`): `0` absent, `1` skeletal bullets, `2` partial map, `3` substantial map with missing edge handling, `4` near-complete with evidence anchors, `5` full operational contract.
- Evidence quality: `low` (claims without reproducible artifacts), `medium` (some measured data/paths), `high` (fixture IDs, commands, and artifact pointers).
- Expansion factor: minimum multiplier required for section rewrite depth in DOC-PASS-01..14.

### 18.2 Section-by-section matrix (`EXHAUSTIVE_LEGACY_ANALYSIS.md`)

| Section | Baseline lines | Coverage | Evidence | Primary omissions to close | Expansion factor | Target lines | Risk | Downstream bead |
|---|---:|---:|---|---|---:|---:|---|---|
| 0. Mission and completion criteria | 9 | 2 | low | Missing measurable closure gates by subsystem and packet | 8x | 72 | medium | `bd-2wb.24.14` |
| 1. Source-of-truth crosswalk | 16 | 2 | medium | Missing commit/hash pinning, path-level ownership, and replay entrypoints | 10x | 160 | high | `bd-2wb.24.2` |
| 2. Quantitative legacy inventory | 12 | 2 | medium | Missing per-command-family, per-test-family, and per-risk-bucket counts | 12x | 144 | high | `bd-2wb.24.1` |
| 3. Subsystem extraction matrix | 13 | 2 | low | Missing module ownership boundaries, coupling direction, and explicit handoff contracts | 15x | 195 | critical | `bd-2wb.24.2` |
| 4. Invariant ledger | 14 | 2 | medium | Missing invariant-to-unit/e2e mapping and failure reason-code taxonomy | 14x | 196 | critical | `bd-2wb.24.4` |
| 5. Native/OS boundary register | 9 | 2 | low | Missing syscall-level failure envelopes and deterministic recovery constraints | 12x | 108 | high | `bd-2wb.24.6` |
| 6. Compatibility/security doctrine | 13 | 2 | medium | Missing strict/hardened allowlist classes, budgeted fallback triggers, and operator actions | 14x | 182 | critical | `bd-2wb.24.6` |
| 7. Conformance program | 21 | 2 | low | Missing per-fixture coverage map, closure criteria, and CI contract | 16x | 336 | critical | `bd-2wb.24.10` |
| 7.1 Fixture families | 9 | 1 | low | Missing fixture IDs, seeds, replay commands, and packet linkage | 18x | 162 | critical | `bd-2wb.24.10` |
| 7.2 Differential harness outputs | 10 | 1 | low | Missing schema version, required fields, and triage reason-code map | 18x | 180 | critical | `bd-2wb.24.10` |
| 8. Extreme optimization program | 24 | 2 | low | Missing measured baseline/profile artifacts and one-lever audit trails | 12x | 288 | high | `bd-2wb.24.7` |
| 9. RaptorQ artifact contract | 13 | 2 | low | Missing concrete manifest schema and decode-proof validation checklist | 14x | 182 | high | `bd-2wb.24.8` |
| 10. Phase-2 execution backlog | 21 | 3 | medium | Missing dependency DAG, owner boundaries, and acceptance evidence links | 10x | 210 | medium | `bd-2wb.24.9` |
| 11. Residual gaps/risks | 6 | 1 | low | Missing quantified risk table with probability/impact and trigger thresholds | 20x | 120 | high | `bd-2wb.24.11` |
| 12. Deep-pass hotspot inventory | 21 | 3 | medium | Missing hotspot-to-lever mapping and counterfactual validation plan | 10x | 210 | medium | `bd-2wb.24.5` |
| 13. Packet extraction payload contract | 21 | 3 | medium | Missing packet schema examples, claim/evidence IDs, and artifact validation rules | 12x | 252 | high | `bd-2wb.24.9` |
| 14. Drift budgets | 15 | 2 | medium | Missing per-family statistical thresholds and escalation policy | 12x | 180 | critical | `bd-2wb.24.6` |
| 15. Optimization execution law | 18 | 2 | medium | Missing profiling protocol details and behavior-isomorphism checklist | 10x | 180 | high | `bd-2wb.24.7` |
| 16. RaptorQ topology/recovery drills | 16 | 2 | low | Missing symbol-generation policy and integrity scrub report schema | 14x | 224 | high | `bd-2wb.24.8` |
| 17. Exit checklist | 9 | 2 | low | Missing objective pass/fail formulas and per-packet attestation format | 12x | 108 | high | `bd-2wb.24.14` |

Baseline slice assessed here (sections `0..17` before DOC-PASS-00 instrumentation): `276` lines.  
Target range for final rewrite: `3,000-3,700` lines (about `10.9x-13.4x`).

### 18.3 Companion matrix status (`EXISTING_REDIS_STRUCTURE.md`)

DOC-PASS-00 coverage for the companion document is now tracked in:
- `EXISTING_REDIS_STRUCTURE.md` section `8.2` (per-section baseline matrix)
- `EXISTING_REDIS_STRUCTURE.md` section `8.3` (completion gates)

Cross-document quantitative snapshot:

| Document | Baseline lines | Target range | Expansion multiple | Highest-risk omissions |
|---|---:|---:|---:|---|
| `EXHAUSTIVE_LEGACY_ANALYSIS.md` (sections `0..17`) | 276 | 3,000-3,700 | 10.9x-13.4x | ownership/coupling map, invariant-to-test traceability, strict/hardened failure envelopes |
| `EXISTING_REDIS_STRUCTURE.md` | 68 | 780-960 | 11.5x-14.1x | topology ownership map, command-surface classification, fixture/reason-code linkage |
| Combined program target | 344 | 3,780-4,660 | 11.0x-13.5x | full parity documentation closure across architecture, behavior, and verification evidence |

### 18.4 High-risk omissions to front-load (P0 first)

1. Missing module/package ownership and dependency direction map for all parity-critical subsystems.  
   Blocks: `bd-2wb.24.2`
2. Missing symbol/API census with public-contract vs incidental-internal classification.  
   Blocks: `bd-2wb.24.3`
3. Missing invariant crosswalk from claims -> unit tests -> e2e scenarios -> forensic logs.  
   Blocks: `bd-2wb.24.4`, `bd-2wb.24.10`
4. Missing control-flow narratives for request lifecycle, replay path, and replication transitions.  
   Blocks: `bd-2wb.24.5`
5. Missing strict/hardened failure envelope taxonomy with explicit reason codes and failover triggers.  
   Blocks: `bd-2wb.24.6`
6. Missing measured optimization evidence pack (baseline/profile/one-lever/re-profile/isomorphism proof).  
   Blocks: `bd-2wb.24.7`
7. Missing RaptorQ manifest/integrity/decode-proof schemas and recovery drill acceptance checks.  
   Blocks: `bd-2wb.24.8`

### 18.5 Completion gates for DOC-PASS-00

DOC-PASS-00 is considered complete when all conditions hold:
1. Every top-level section and nested conformance subsection in both target docs has a baseline line count, coverage score, omissions list, and numeric target.
2. Per-document and combined target ranges are explicit and reproducible from repository content.
3. P0 omissions are prioritized and linked to downstream beads with dependency-auditable IDs.
4. Matrices are auditable from repository content alone (no hidden assumptions).

## 19. DOC-PASS-11 Expansion Draft (Pass B): Behavioral Semantics, Risks, and Verification Bindings

### 19.1 Scope and draft intent

Pass B expands beyond inventory into behavior contracts:
1. packet-level semantics and failure envelopes,
2. strict/hardened decision boundaries under hostile inputs,
3. explicit unit/e2e/logging bindings and replay paths,
4. risk-ranked gap ledger linked to concrete follow-up beads.

Cross-document coupling for this pass:
- architecture/control-flow/error baselines are anchored in `EXISTING_REDIS_STRUCTURE.md` sections `17..24`,
- schema/logging contract is anchored in `TEST_LOG_SCHEMA_V1.md`.

### 19.2 Packet-level behavior and risk matrix (Pass-B draft)

| Packet | Behavioral contract focus | High-risk failure classes | Strict/Hardened envelope | Verification/log anchors | Status + gap bead |
|---|---|---|---|---|---|
| `FR-P2C-001` Event loop core | deterministic phase ordering, bounded blocked-mode budget, bootstrap prerequisites | phase-order drift, partial tick traces, missing hooks/timers | strict: fail-closed on any trace/bootstrap violation; hardened: same external contract with bounded diagnostics only | `crates/fr-eventloop/src/lib.rs`, `crates/fr-runtime/src/lib.rs`, `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-001.golden.jsonl` | implemented (`bd-2wb.12.7` closed) |
| `FR-P2C-002` RESP parser contract | malformed framing rejection and protocol-error string contract | parse desync, invalid length handling, transport timeout ambiguity on malformed payloads | strict: reject malformed frame with deterministic reason; hardened: bounded parser defense, no semantic relaxation | `crates/fr-protocol/src/lib.rs`, `crates/fr-runtime/src/lib.rs`, `crates/fr-conformance/fixtures/protocol_negative.json`, `artifacts/e2e_orchestrator/*/suites/protocol_negative/report.json` | implemented (`bd-2wb.13.7` closed) |
| `FR-P2C-003` Dispatch core | command routing determinism and arity/syntax normalization | command-family drift under incremental feature growth | strict: no behavior-altering fallback; hardened: bounded reject policy only | `crates/fr-command/src/lib.rs`, `crates/fr-conformance/fixtures/core_errors.json` | implemented (`bd-2wb.14.7` closed) |
| `FR-P2C-004` ACL/auth policy | pre-dispatch auth gates, AUTH/HELLO transitions, wrongpass handling | auth bypass/order inversion, ACL surface incompleteness | strict: `NOAUTH`/`WRONGPASS` fail-closed; hardened: identical observable auth contract + richer diagnostics | `crates/fr-runtime/src/lib.rs`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-004/*.md`, `log_contract_v1/FR-P2C-004.golden.jsonl` | implemented (`bd-2wb.15.7` closed) |
| `FR-P2C-005` Persistence/replay | deterministic replay ordering and post-replay assertions | decode corruption, replay ordering drift, AOF tail ambiguity | strict: fail on untrusted/decode-invalid replay segments; hardened: bounded recovery only where allowlisted | `crates/fr-persist/src/lib.rs`, `crates/fr-conformance/fixtures/persist_replay.json`, `log_contract_v1/FR-P2C-005.golden.jsonl` | implemented (`bd-2wb.16.7` closed) |
| `FR-P2C-006` Replication | handshake FSM and PSYNC fallback safety | handshake reorder attacks, offset-window abuse, stale ACK regression | strict: reject invalid transitions; hardened: bounded full-resync fallback, never unsafe partial continuation | `crates/fr-repl/src/lib.rs`, `crates/fr-conformance/src/lib.rs` (`fr_p2c_006_f_*`), `log_contract_v1/FR-P2C-006.golden.jsonl` | implemented (`bd-2wb.17.7` closed) |
| `FR-P2C-007` Cluster (scoped) | deterministic subcommand routing and client cluster mode flags | redirect/routing ambiguity, unsupported subcommand surfaces | strict: reject unknown/unsupported paths; hardened: same API contract with bounded defensive classification | `crates/fr-runtime/src/lib.rs`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-007/*.md`, `log_contract_v1/FR-P2C-007.golden.jsonl` | e2e implemented (`bd-2wb.18.7` closed); deferred-action scope remains bounded |
| `FR-P2C-008` Expire/evict | TTL correctness under pressure and active-expire parity intent | lazy-vs-active expiry drift, eviction-policy under-specification | strict: preserve current explicit semantics, fail closed on undefined policy paths; hardened: bounded pressure defenses only | `crates/fr-expire/src/lib.rs`, `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/*.md`, `log_contract_v1/FR-P2C-008.golden.jsonl` | partial; packet E2E suite pending `bd-2wb.19.7` |
| `FR-P2C-009` TLS/config | fail-closed config validation and hardened allowlist gate | config downgrade abuse, invalid runtime apply transitions | strict: reject unsafe config, record deterministic reason code; hardened: allowlist-only bounded defense + reject non-allowlisted deviations | `crates/fr-config/src/lib.rs`, `crates/fr-runtime/src/lib.rs`, `crates/fr-conformance/src/lib.rs` (`fr_p2c_009_e013_*`), `log_contract_v1/FR-P2C-009.golden.jsonl` | implemented (`bd-2wb.20.7` closed) |

### 19.3 High-risk claim bindings (strict/hardened replay + forensic fields)

| Claim ID | High-risk claim | Strict replay command | Hardened replay command | Expected forensic fields |
|---|---|---|---|---|
| `CLAIM-PARSER-001` | Malformed protocol input never mutates state and yields deterministic parser failure class | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture protocol_invalid_bulk_length_error_string` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture conformance_protocol_fixture_passes` | `suite_id`, `test_or_scenario_id`, `packet_id`, `mode`, `reason_code`, `replay_cmd`, `artifact_refs` |
| `CLAIM-AUTH-001` | `NOAUTH` gate executes before dispatch under protected runtime | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u005_noauth_gate_runs_before_dispatch` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u004_hello_auth_early_fails_and_success_path_authenticates` | `reason_code`, `outcome`, `input_digest`, `output_digest`, `artifact_refs` |
| `CLAIM-REPL-001` | Invalid handshake transitions are rejected without state advancement | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u003_handshake_requires_ping_first` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_handshake_contract_vectors_are_enforced` | `mode`, `reason_code`, `replay_cmd`, `artifact_refs`, `duration_ms` |
| `CLAIM-TLS-001` | Non-allowlisted hardened TLS deviations are rejected deterministically | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u013_strict_mode_rejects_unsafe_tls_config_and_records_event` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract` | `threat_class`, `decision_action`, `reason_code`, `replay_cmd`, `artifact_refs` |
| `CLAIM-LIVE-001` | Live differential runner emits replayable failure bundle without suppressing parity drift | `./scripts/run_live_oracle_diff.sh --host 127.0.0.1 --port 6379 --run-id <id>` | `FR_E2E_RUNNER=rch ./scripts/run_live_oracle_diff.sh --host <reachable-host> --port <port> --run-id <id>` | `suite_status.tsv`, `command_trace.log`, per-suite `report.json`, `replay_failed.sh`, `live_logs/*.jsonl` |

### 19.4 Alien-graveyard recommendation contract card (Pass-B draft)

| Field | Value |
|---|---|
| `card_id` | `AG-FR-PASSB-001` |
| Hotspot evidence | Live differential bundle evidence shows deterministic report emission and visible parity drift classes (`artifacts/e2e_orchestrator/bd-2wb6-validation-local-20260216T1540Z/suite_status.tsv`, `.../suites/core_errors/report.json`, `.../suites/protocol_negative/report.json`). |
| Mapped graveyard section IDs | `AG-DET-04` (deterministic reducer), `AG-AUD-03` (forensic logging contract), `AG-SEC-11` (fail-closed admission gates). |
| Baseline comparator | local live Redis differential run against `core_errors/core_strings/protocol_negative`. |
| EV score | `2.7` (high signal + low implementation friction for next reliability gate work). |
| Priority tier | `S` |
| Adoption wedge | Promote bundle schema (`suite_status.tsv` + JSON reports + replay script) as canonical failure envelope for CI gate topology (`bd-2wb.10`). |
| Budgeted-mode default | strict path remains fail-closed; hardened path allows only allowlisted bounded defenses and still emits full forensic trail. |
| Exhaustion behavior | deterministic `execution_error` report payload on transport/setup failures; no silent swallow/retry loops. |
| Claim/evidence linkage | `claim_id`: `CLAIM-LIVE-001`; `evidence_id`: `EVID-LIVE-BUNDLE-20260216T1540Z`; replay path embedded in bundle `replay_failed.sh`. |

### 19.5 Expected-loss decision model (risk/recovery control)

State set:
- `S0`: parity pass,
- `S1`: parity drift with deterministic artifact capture,
- `S2`: execution-path failure (transport/setup/runtime crash),
- `S3`: nondeterministic/insufficient forensic evidence.

Action set:
- `A0`: accept run and advance,
- `A1`: block promotion and file targeted follow-up bead,
- `A2`: rerun under hardened mode with bounded diagnostics,
- `A3`: fail closed and escalate with replay bundle.

Loss matrix (lower is better):

| State \ Action | `A0` | `A1` | `A2` | `A3` |
|---|---:|---:|---:|---:|
| `S0` parity pass | 0 | 2 | 3 | 5 |
| `S1` deterministic drift | 6 | 1 | 2 | 3 |
| `S2` execution failure | 8 | 3 | 4 | 1 |
| `S3` missing forensics | 9 | 4 | 5 | 1 |

Calibration and fallback trigger:
- calibration metric: agreement between rerun classification and original classification (`>= 0.9` target over rolling window),
- fallback trigger: if `S2` or `S3` is observed, force `A3` and block promotion until replay bundle is attached to the bead thread.

### 19.6 One-lever optimization protocol (next binding)

Selected lever candidate for next cycle:
- **Lever:** bounded protocol negative-case socket read policy in live differential runner to reduce timeout-class ambiguity while preserving fail-closed semantics.

Required loop artifacts (next bead chain):
1. baseline from current bundle (`bd-2wb6-validation-local-20260216T1540Z`),
2. profile/trace of timeout-class failures,
3. single lever implementation,
4. conformance/isomorphism verification,
5. re-baseline delta report.

### 19.7 Reproducibility/provenance pack references

Current provenance anchors:
- `crates/fr-conformance/fixtures/log_contract_v1/env.json`
- `crates/fr-conformance/fixtures/log_contract_v1/manifest.json`
- `crates/fr-conformance/fixtures/log_contract_v1/repro.lock`
- `artifacts/e2e_orchestrator/bd-2wb6-validation-local-20260216T1540Z/README.md`
- `artifacts/e2e_orchestrator/bd-2wb6-validation-local-20260216T1540Z/replay_failed.sh`

IP/legal note:
- no new third-party code ingestion in this pass; `LEGAL.md` escalation remains contingent on future external corpus imports.

### 19.8 Pass-B gap ledger and dependency map

| Gap | Impact | Follow-up bead |
|---|---|---|
| Packet-specific E2E suite for `FR-P2C-008` remains open | prevents full packet-family closure for Expire/Evict journey | `bd-2wb.19.7` |
| Packet-008 differential/adversarial and optimization evidence still pending | keeps one packet chain from final end-to-end completion | `bd-2wb.19.6`, `bd-2wb.19.8`, `bd-2wb.19.9` |
| CI gate topology now formalized and active (`G1..G8` + forensics index) | requires ongoing schema stability and artifact contract discipline | implemented via closed `bd-2wb.10` |
| Coverage/flake budget policy and forensics operator index now landed | shifts risk from “missing foundation” to “packet residual depth” | implemented via closed `bd-2wb.23` + `bd-2wb.22` |
| Red-team contradiction review and specialist deep passes | no longer pending; integrated outputs now feed final doc pass | implemented via closed `bd-2wb.24.13`, `bd-2wb.24.16`, `bd-2wb.24.17` |

## 20. DOC-PASS-12 Independent Red-Team Contradiction and Completeness Review

### 20.1 Red-team method and evidence gate

Independent review rule used in this pass:
1. treat each non-trivial claim as false until source evidence is found;
2. classify every mismatch as either contradiction, ambiguity, or verified claim;
3. either resolve mismatch directly in docs or bound it with explicit follow-up bead IDs.

Deterministic evidence commands used for contradiction checks:
- `rg -n "fr_persist|fr_repl|AofRecord|ReplProgress" crates/fr-runtime/src/lib.rs`
- `rg -n "evaluate_expiry|fr_expire" crates`
- `rg -n "Deferred TODO|TODO" crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md`
- `rg -n "fr_p2c_001_u001_phase_order_is_deterministic|fr_p2c_001_u010_bootstrap_rejects_missing_hooks|fr_p2c_001_u005_runtime_blocked_mode_is_bounded|conformance_protocol_fixture_passes|fr_p2c_007_u001_cluster_subcommand_router_is_deterministic|fr_p2c_007_u007_client_cluster_mode_flags_transition_cleanly|conformance_replay_fixture_passes|run_replay_fixture_allows_structured_log_persistence_toggle|fr_p2c_006_f_psync_adversarial_matrix_prefers_safe_fallbacks|fr_p2c_009_u013_hardened_gate_rejects_non_allowlisted_deviation|fr_p2c_009_u013_hardened_non_allowlisted_tls_deviation_is_rejected" crates`

### 20.2 Contradiction and ambiguity register (adversarial pass)

| ID | Contradiction / ambiguity | Source-grounded evidence | Severity | Disposition and bounded resolution |
|---|---|---|---|---|
| `RT-001` | Architecture narrative implies end-to-end persistence/replication flow, but runtime command path currently dispatches only to store. | `crates/fr-runtime/src/lib.rs` `execute_frame` path calls `dispatch_argv(&argv, &mut self.store, now_ms)` and contains no `fr_persist`/`fr_repl` references; `crates/fr-persist/src/lib.rs` and `crates/fr-repl/src/lib.rs` currently provide standalone kernels. | high | Resolved as explicit scope bound: persistence/replication are contract targets, not runtime-integrated in current state. Promotion remains blocked by `bd-2wb.16`, `bd-2wb.16.7`, `bd-2wb.17`, `bd-2wb.17.7`. |
| `RT-002` | Expiration subsystem appears as crate-level component, but active-expire orchestration is not wired through runtime/store scheduling. | `crates/fr-expire/src/lib.rs` contains helper kernel `evaluate_expiry`; `crates/fr-store/src/lib.rs` contains current lazy expiry behavior (`drop_if_expired`) and TTL logic; packet plan notes helper-only status in `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/implementation_plan.md`. | high | Resolved as explicit caveat: `FR-P2C-008` claims remain bounded to currently implemented lazy semantics plus documented gaps. Follow-up remains `bd-2wb.19`, `bd-2wb.19.7`. |
| `RT-003` | Cluster packet contract includes deferred `clusterBeforeSleep` actions, while runtime cluster implementation remains intentionally narrow. | Deferred contract entry: `crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md` row `FR-P2C-007-C13`; runtime currently exposes `CLUSTER HELP` scaffold and client mode toggles in `crates/fr-runtime/src/lib.rs`. | high | Resolved as bounded uncertainty: no completeness claim for cluster packet is permitted until deferred actions are implemented and replay-tested. Follow-up remains `bd-2wb.18`, `bd-2wb.18.7`. |
| `RT-004` | RaptorQ-everywhere doctrine is broader than any single packet slice; risk was missing foundation-wide gate wiring. | Foundation sidecar + decode-proof pipeline is now wired (`scripts/run_raptorq_artifact_gate.sh`) and consumed by CI gate topology (`.github/workflows/live-conformance-gates.yml`) with deterministic artifacts. | medium | Resolved at foundation scope: `bd-2wb.9` + `bd-2wb.10` are closed; continue extending artifact coverage as new durability bundles are introduced. |
| `RT-005` | Packet E2E depth remains uneven after broad closure, with residual concentration in `FR-P2C-008`. | Crosswalk in `EXISTING_REDIS_STRUCTURE.md` section `24.2` shows packet-specific E2E closures for `FR-P2C-001/002/003/004/005/006/007/009`, with residual open `FR-P2C-008` (`bd-2wb.19.7`). | medium | Resolved by preserving explicit residual-gap ledger and preventing final sign-off until packet-008 E2E evidence is closed. |

### 20.3 Unsupported-claim reconciliation actions (applied)

| Action ID | Claim class | Action taken | Verification / trace |
|---|---|---|---|
| `RC-001` | Persistence/replication integration overstatement risk | Qualified as “contract target, not yet runtime-integrated.” | `RT-001` + packet beads `bd-2wb.16*`, `bd-2wb.17*` |
| `RC-002` | Expire subsystem completeness overstatement risk | Qualified as “helper kernel + lazy semantics currently; active-expire parity pending.” | `RT-002` + packet beads `bd-2wb.19*` |
| `RC-003` | Cluster completeness overstatement risk | Qualified as “scaffold + deferred contract actions pending.” | `RT-003` + packet beads `bd-2wb.18*` |
| `RC-004` | Replay-command existence uncertainty | Verified all listed high-risk replay symbols exist in source. | evidence command in section `20.1` (symbol existence grep) |

### 20.4 Completeness stress-test matrix (unit/e2e/log depth)

| Surface | Unit/property status | E2E status | Logging status | Bound and follow-up |
|---|---|---|---|---|
| `FR-P2C-005` persistence/replay | present (`conformance_replay_fixture_passes`) | packet-specific E2E implemented | golden + optional live JSONL paths present | implemented (`bd-2wb.16.7` closed) |
| `FR-P2C-006` replication | present (FSM/PSYNC reducers + fixture vectors) | packet-specific E2E implemented | packet golden logs present | implemented (`bd-2wb.17.7` closed) |
| `FR-P2C-007` cluster | present (router/client mode unit tests; deferred actions still explicitly bounded) | packet-specific E2E implemented | packet golden logs present | e2e implemented (`bd-2wb.18.7` closed); deferred action scope remains in packet chain |
| `FR-P2C-008` expire/evict | partial (store lazy-expiry + helper kernel) | packet-specific E2E still open | packet golden logs present | bounded by open `bd-2wb.19.7` |
| `FR-P2C-009` tls/config | present (strict/hardened gate tests + conformance case) | packet-specific E2E implemented | packet golden logs present | implemented (`bd-2wb.20.7` closed) |

### 20.5 Carry-forward contract verification (alien + expected-loss + one-lever)

This pass re-validates and carries forward Pass-B contract artifacts:
1. recommendation contract card remains `AG-FR-PASSB-001` (section `19.4`) with EV `2.7` and evidence linkage (`claim_id`/`evidence_id`);
2. expected-loss model remains section `19.5`, with explicit fallback trigger `S2|S3 -> A3 fail-closed`;
3. one-lever optimization loop remains section `19.6`; foundation perf/gate prerequisites are closed, and residual work is concentrated in packet-008 optimization/final evidence (`bd-2wb.19.8`, `bd-2wb.19.9`).

### 20.6 Bounded uncertainty policy (no handwaving)

Uncertainty is considered bounded only if all conditions hold:
1. explicit risk statement exists with source anchors;
2. strict/hardened external behavior boundary is documented;
3. replay command and expected forensic fields are specified;
4. a blocking follow-up bead ID exists and remains open until evidence lands.

Items failing any condition above are contradictions, not TODOs, and cannot be treated as “known gaps.”

### 20.7 Red-team acceptance evidence pack

Verification commands for this pass:
- `UBS_MAX_DIR_SIZE_MB=5000 ubs --diff`
- `rg -n "fr_persist|fr_repl|AofRecord|ReplProgress" crates/fr-runtime/src/lib.rs`
- `rg -n "evaluate_expiry|fr_expire" crates`
- `rg -n "Deferred TODO|TODO" crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md`

Operational replay anchors (carry-forward from Pass B):
- strict parser gate: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture protocol_invalid_bulk_length_error_string`
- hardened protocol fixture: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture conformance_protocol_fixture_passes`
- strict auth gate: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u005_noauth_gate_runs_before_dispatch`
- hardened tls allowlist gate: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract`

Pass verdict:
- Contradictions and ambiguities are explicitly listed and dispositioned.
- Unsupported claims were either evidenced or qualified with hard bounds and blocking beads.
- Remaining uncertainty is explicit, replayable, and dependency-tracked.

## 21. DOC-PASS-15 Full-Agent Deep Dive Pass B (Behavior Specialist)

### 21.1 Behavior-specialist audit scope

This pass focuses on behavior semantics and edge-condition interpretation:
1. validate invariant narratives against code and tests,
2. clarify strict/hardened boundaries where behavior could be over-read,
3. strengthen unit/e2e/log traceability for high-risk behavior claims,
4. preserve unresolved behavior uncertainty as explicit, bounded backlog items.

### 21.2 Invariant validation matrix (behavior semantics)

| Invariant ID | Behavioral invariant | Unit/property anchors | E2E/differential anchors | Log/forensics anchors | Verdict |
|---|---|---|---|---|---|
| `BHV-001` | malformed protocol input fails before dispatch and returns deterministic protocol error class | `crates/fr-runtime/src/lib.rs` (`protocol_invalid_bulk_length_error_string`) | `crates/fr-conformance/src/lib.rs` (`conformance_protocol_fixture_passes`), `crates/fr-conformance/fixtures/protocol_negative.json` | `crates/fr-conformance/src/log_contract.rs` (`reason_code`, `replay_cmd`, `artifact_refs`) | validated |
| `BHV-002` | compatibility gate (`max_array_len` / `max_bulk_len`) executes pre-dispatch and fail-closes | `crates/fr-runtime/src/lib.rs` (`compatibility_gate_trips_on_large_array`) | `protocol_negative` fixture + live differential suite report (`artifacts/e2e_orchestrator/<run-id>/suites/protocol_negative/report.json`) | packet log family `FR-P2C-002.golden.jsonl` | validated |
| `BHV-003` | `NOAUTH` gate runs before command execution; HELLO+AUTH path transitions deterministically | `crates/fr-runtime/src/lib.rs` (`fr_p2c_004_u004_*`, `fr_p2c_004_u005_*`) | packet-specific E2E implemented (`fr_p2c_004_e2e_contract_smoke`) | `FR-P2C-004.golden.jsonl`, threat reason codes in runtime evidence | implemented (`bd-2wb.15.7` closed) |
| `BHV-004` | cluster command behavior is deterministic for implemented subset (HELP, mode flags), and rejects unknown paths | `crates/fr-runtime/src/lib.rs` (`fr_p2c_007_u001_*`, `fr_p2c_007_u007_*`) | packet-specific cluster E2E implemented (`fr_p2c_007_e2e_contract_smoke`) | `FR-P2C-007.golden.jsonl` | implemented for scoped subset (`bd-2wb.18.7` closed); deferred actions remain bounded |
| `BHV-005` | TTL behavior currently follows lazy expiry semantics (`PTTL`/deadline handling) | `crates/fr-store/src/lib.rs` (`expire_and_pttl`, `expire_*`, `drop_if_expired`) | foundation fixture coverage; packet-specific E2E pending | `FR-P2C-008.golden.jsonl` | validated for current scope; active-expire/eviction parity pending (`bd-2wb.19.7`) |
| `BHV-006` | replication safety reducers preserve ordering and safe fallback (`handshake` sequencing + PSYNC rejection paths) | `crates/fr-repl/src/lib.rs` (`fr_p2c_006_u003_*`, `fr_p2c_006_u002_*`) | `crates/fr-conformance/src/lib.rs` (`fr_p2c_006_f_*`) | `FR-P2C-006.golden.jsonl` | validated as kernel behavior; runtime integration pending |
| `BHV-007` | strict/hardened TLS/config gates reject unsafe or non-allowlisted deviations deterministically | `crates/fr-runtime/src/lib.rs` (`fr_p2c_009_u013_*`), `crates/fr-config/src/lib.rs` (`fr_p2c_009_u013_hardened_gate_rejects_non_allowlisted_deviation`) | `crates/fr-conformance/src/lib.rs` (`fr_p2c_009_e013_*`) | `FR-P2C-009.golden.jsonl` | validated |

### 21.3 Ambiguity clarifications incorporated

1. Persistence and replication behavior claims are explicitly treated as contract targets until runtime wiring exists (`RT-001` carry-forward).
2. Expiration behavior is explicitly bounded to current lazy-expiry semantics; active-expire/eviction orchestration is still pending (`RT-002` carry-forward).
3. Cluster behavior claims are restricted to currently implemented subset; deferred `clusterBeforeSleep` actions are not treated as implemented behavior (`RT-003` carry-forward).
4. Packet-level “partial” status cannot be upgraded without packet-specific E2E evidence and matching structured-log artifacts.

### 21.4 High-risk behavior replay matrix (strict/hardened + forensic fields)

| Claim ID | Behavior assertion | Strict replay command | Hardened replay command | Required forensic fields |
|---|---|---|---|---|
| `BHV-CLAIM-001` | parser failures remain fail-closed and deterministic | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture protocol_invalid_bulk_length_error_string` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture conformance_protocol_fixture_passes` | `suite_id`, `packet_id`, `mode`, `reason_code`, `replay_cmd`, `artifact_refs` |
| `BHV-CLAIM-002` | auth admission ordering remains pre-dispatch (`NOAUTH` before command execution) | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u005_noauth_gate_runs_before_dispatch` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_004_u004_hello_auth_early_fails_and_success_path_authenticates` | `outcome`, `reason_code`, `input_digest`, `output_digest`, `artifact_refs` |
| `BHV-CLAIM-003` | replication handshake and PSYNC reducers prefer safe fallback over unsafe continuation | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u003_handshake_requires_ping_first` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_psync_adversarial_matrix_prefers_safe_fallbacks` | `packet_id`, `mode`, `reason_code`, `duration_ms`, `replay_cmd` |
| `BHV-CLAIM-004` | cluster mode transitions for implemented subset are deterministic and reversible | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_007_u001_cluster_subcommand_router_is_deterministic` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_007_u007_client_cluster_mode_flags_transition_cleanly` | `packet_id`, `mode`, `outcome`, `reason_code`, `artifact_refs` |
| `BHV-CLAIM-005` | strict/hardened TLS policy gates remain fail-closed on unsafe/non-allowlisted transitions | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u013_strict_mode_rejects_unsafe_tls_config_and_records_event` | `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract` | `threat_class`, `decision_action`, `reason_code`, `replay_cmd`, `artifact_refs` |

### 21.5 Behavior recommendation contract card (alien-graveyard carry-forward)

| Field | Value |
|---|---|
| `card_id` | `AG-FR-BEH-001` |
| Hotspot evidence | Behavioral drift risk concentrates in runtime admission/dispatch ordering and packet-subset cluster surfaces (`crates/fr-runtime/src/lib.rs`, `crates/fr-command/src/lib.rs`). |
| Mapped graveyard section IDs | `AG-DET-04` (deterministic semantics), `AG-AUD-03` (forensic replayability), `AG-VAL-06` (contract validation pressure). |
| Baseline comparator | strict/hardened replay matrix in section `21.4` + live differential bundle schema from section `19.3` (`CLAIM-LIVE-001`). |
| EV score | `2.6` |
| Priority tier | `A` |
| Adoption wedge | enforce behavior-claim admission rule: no high-risk claim accepted without one strict replay, one hardened replay, and one structured-log artifact pointer. |
| Budgeted-mode default | strict remains fail-closed; hardened may add bounded diagnostics only where allowlisted. |
| Exhaustion behavior | missing or contradictory evidence triggers fail-closed escalation to contradiction register and blocks closure. |
| Claim/evidence linkage | `claim_id`: `BHV-CLAIM-003`; `evidence_id`: `EVID-BEH-REPL-PSYNC-20260216`; replay commands in section `21.4`. |

### 21.6 Expected-loss decision model (behavior drift triage)

State set:
- `B0`: behavior match with complete evidence,
- `B1`: deterministic behavior mismatch with complete evidence,
- `B2`: behavior mismatch with missing or weak forensic evidence,
- `B3`: ambiguous strict/hardened boundary interpretation.

Action set:
- `D0`: accept and advance,
- `D1`: block and file/update targeted follow-up bead,
- `D2`: rerun strict+hardened with deterministic seed and artifact capture,
- `D3`: fail closed and escalate contradiction.

Loss matrix (lower is better):

| State \ Action | `D0` | `D1` | `D2` | `D3` |
|---|---:|---:|---:|---:|
| `B0` match + evidence | 0 | 2 | 3 | 5 |
| `B1` mismatch + evidence | 6 | 1 | 2 | 2 |
| `B2` mismatch + weak evidence | 9 | 4 | 3 | 1 |
| `B3` ambiguous boundary | 8 | 3 | 2 | 1 |

Calibration target:
- rerun agreement (`strict+hardened`) on classification outcome `>= 0.9` over rolling packet windows.
Fallback trigger:
- any `B2`/`B3` classification forces `D3` until contradiction is resolved or explicitly bounded with blocking bead.

### 21.7 One-lever optimization loop (behavior-focused)

Chosen lever candidate:
- add deterministic command-path signature capture (`preflight -> auth/cluster gate -> dispatch`) per high-risk conformance case to reduce interpretation drift.

Required artifacts:
1. baseline: current behavior matrix + existing conformance outcomes,
2. profile: identify behavior-ambiguity hotspots by packet and reason_code class,
3. one lever: signature capture and report emission,
4. proof: strict/hardened behavior isomorphism replay,
5. re-baseline: ambiguity-rate delta report.

Follow-up dependency targets:
- packet-008 closure chain: `bd-2wb.19.5`, `bd-2wb.19.6`, `bd-2wb.19.7`, `bd-2wb.19.8`, `bd-2wb.19.9`.

### 21.8 Reproducibility and provenance references

- `crates/fr-conformance/fixtures/log_contract_v1/env.json`
- `crates/fr-conformance/fixtures/log_contract_v1/manifest.json`
- `crates/fr-conformance/fixtures/log_contract_v1/repro.lock`
- `artifacts/e2e_orchestrator/<run-id>/suite_status.tsv`
- `artifacts/e2e_orchestrator/<run-id>/replay_failed.sh`

Legal/IP note:
- no additional external corpus imported in this pass; `LEGAL.md` escalation remains contingent on future third-party behavior corpus ingestion.

### 21.9 Behavior gap ledger for handoff

| Gap ID | Behavior gap | Blocking bead(s) |
|---|---|---|
| `BHV-G01` | Runtime integration for persistence/replication behavior path | `bd-2wb.16`, `bd-2wb.16.7`, `bd-2wb.17`, `bd-2wb.17.7` |
| `BHV-G02` | Active-expire/eviction orchestration parity beyond lazy semantics | `bd-2wb.19`, `bd-2wb.19.7` |
| `BHV-G03` | Cluster deferred-task behavioral contract execution | `bd-2wb.18`, `bd-2wb.18.7` |
| `BHV-G04` | Residual packet-specific E2E depth (expire/evict journey) | `bd-2wb.19.7` |
| `BHV-G05` | CI-level behavior-gate ingestion of deterministic failure envelope artifacts | implemented via closed `bd-2wb.10` + `bd-2wb.23`; monitor packet residual depth via `bd-2wb.19.7` |

## 22. DOC-PASS-16 Full-Agent Deep Dive Pass C (Risk/Perf/Test Specialist)

### 22.1 Tightened risk taxonomy (source anchored)

| Risk ID | Threat / failure surface | Current controls | Residual risk | Mitigation / blocker bead |
|---|---|---|---|---|
| `RPT-001` | malformed protocol frame abuse and parser desync | parser rejection and protocol error path in runtime + strict/hardened gate policy (`crates/fr-runtime/src/lib.rs`, `crates/fr-config/src/lib.rs`) | medium | packet E2E depth + adversarial corpus: `bd-2wb.13.7`, `bd-2wb.7` |
| `RPT-002` | auth admission ordering confusion (`NOAUTH`, `HELLO AUTH`) | explicit auth gate before dispatch + unit tests (`fr_p2c_004_u004/u005`) | medium | packet E2E closure: `bd-2wb.15.7` |
| `RPT-003` | replay/replication ordering and unsafe continuation | replication reducers (`decide_psync`, handshake FSM) + conformance vectors | high | runtime integration + packet E2E: `bd-2wb.17`, `bd-2wb.17.7` |
| `RPT-004` | persistence replay corruption/order drift | `fr-persist` AOF encode/decode kernels + replay fixtures | high | runtime wiring + packet E2E: `bd-2wb.16`, `bd-2wb.16.7` |
| `RPT-005` | cluster deferred action drift and routing ambiguity | deterministic implemented subset only (`CLUSTER HELP`, mode flags) | high | deferred task execution + packet E2E: `bd-2wb.18`, `bd-2wb.18.7` |
| `RPT-006` | TTL/eviction pressure mismatch between lazy and active policies | store lazy-expiry semantics + helper kernel in `fr-expire` | high | active-expire/eviction closure: `bd-2wb.19`, `bd-2wb.19.7` |
| `RPT-007` | TLS/config downgrade and non-allowlisted hardened deviations | policy allowlist + strict/hardened gate tests (`fr_p2c_009_u013_*`, `fr_p2c_009_e013_*`) | medium | packet E2E depth: `bd-2wb.20.7` |
| `RPT-008` | forensic evidence incompleteness (operator cannot replay diagnosis) | structured log schema contract + live bundle artifacts + CI forensics index | low-medium | foundation mitigations closed (`bd-2wb.22`, `bd-2wb.10`); residual packet-depth gap tracked by `bd-2wb.19.7` |

### 22.2 Performance-proof readiness assessment

Current performance/proof anchors:
- baseline artifacts in `README.md`: `baselines/round1_conformance_baseline.json`, `baselines/round2_protocol_negative_baseline.json` (+ strace companions),
- optimization evidence pack present at `artifacts/optimization/phase2c-gate/round_dir_scan_mask/` (`baseline_hyperfine.json`, `after_hyperfine.json`, `optimization_report.md`, `isomorphism_check.txt`, `env.json`, `manifest.json`, `repro.lock`).

Primary readiness gaps:
1. packet-008 chain still has open verification/evidence beads (`bd-2wb.19.6`, `bd-2wb.19.7`, `bd-2wb.19.8`, `bd-2wb.19.9`);
2. final integrated sign-off remains blocked until packet-008 depth closes (`bd-2wb.24.14` dependency on `bd-2wb.19.*`);
3. CI artifact schema must remain stable as additional packet evidence is integrated.

### 22.3 Verification-depth completeness map (unit/e2e/logging)

| Verification lane | Current status | Evidence anchors | Gap / blocker |
|---|---|---|---|
| Unit/property contracts | foundationally strong in key crates (`fr-runtime`, `fr-repl`, `fr-store`, `fr-config`) | packet-tagged unit tests (`fr_p2c_*`) | packet-specific unit/property completion still depends on packet `*.5` and `*.6` chains |
| E2E deterministic scripts | foundation orchestrator exists and emits replayable bundles | `scripts/run_live_oracle_diff.sh`, `live_oracle_diff` binary reports | packet E2E closures landed for `FR-P2C-001/002/003/004/005/006/007/009`; residual open depth is `FR-P2C-008` (`bd-2wb.19.7`) |
| Structured log contract | schema + golden packet logs + conversion validation are implemented | `crates/fr-conformance/src/log_contract.rs`, `TEST_LOG_SCHEMA_V1.md` | CI gate ingestion and operator index are implemented (closed `bd-2wb.10`, `bd-2wb.22`); packet-008 E2E depth remains (`bd-2wb.19.7`) |

### 22.4 CI gate topology readiness (G1..G8)

| Gate | Intent | Current substrate status | Bead status |
|---|---|---|---|
| `G1` | fmt/lint baseline | wired in CI workflow (`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`) | implemented (closed `bd-2wb.10`) |
| `G2` | unit/property deterministic checks | wired in CI workflow (`cargo test --workspace -- --nocapture`) | implemented (closed `bd-2wb.10`) |
| `G3` | differential parity gate | live differential orchestrator wired in CI (`run_live_oracle_diff.sh`) | implemented; residual packet-008 closure depth (`bd-2wb.19.*`) |
| `G4` | adversarial/fuzz gate | adversarial triage pipeline wired in CI | implemented (closed `bd-2wb.7` + `bd-2wb.10`) |
| `G5` | deterministic E2E/orchestrator gate | orchestrator + replay bundle + deterministic artifact uploads wired in CI | implemented foundation; packet-008 E2E still open (`bd-2wb.19.7`) |
| `G6` | perf regression + one-lever proof | optimization schema gate wired in CI (`phase2c_schema_gate -- --optimization-gate`) | implemented foundation (closed `bd-2wb.8` + `bd-2wb.10`) |
| `G7` | artifact schema/forensics contract | coverage/flake budget + corpus/schema gates + failure-forensics index wired in CI | implemented foundation (closed `bd-2wb.22`, `bd-2wb.23`, `bd-2wb.10`) |
| `G8` | RaptorQ decode-proof gate | RaptorQ artifact gate wired in CI with deterministic report artifacts | implemented foundation (closed `bd-2wb.9` + `bd-2wb.10`) |

### 22.5 Risk/Perf/Test recommendation contract card

| Field | Value |
|---|---|
| `card_id` | `AG-FR-RPT-001` |
| Hotspot evidence | Foundation blockers are closed; dominant residual risk is packet-008 completion depth (`bd-2wb.19`, `bd-2wb.19.5`, `bd-2wb.19.6`, `bd-2wb.19.7`, `bd-2wb.19.8`, `bd-2wb.19.9`). |
| Mapped graveyard section IDs | `AG-SEC-11` (fail-closed policy), `AG-PERF-08` (one-lever proof discipline), `AG-AUD-03` (deterministic forensic trails). |
| Baseline comparator | current documented baseline/perf artifact set + live differential bundle schema. |
| EV score | `2.9` |
| Priority tier | `S` |
| Adoption wedge | make gate-readiness state machine explicit in docs and require blocker-to-gate mapping for every unresolved lane. |
| Budgeted-mode default | strict mode blocks promotion on unresolved high-risk lanes; hardened mode cannot waive missing evidence contracts. |
| Exhaustion behavior | if blocker set is unchanged across review cycle, automatically escalate to go/no-go “NO-GO” until at least one critical blocker is closed. |
| Claim/evidence linkage | `claim_id`: `RPT-CLAIM-GATE-001`; `evidence_id`: `EVID-RPT-GATE-MATRIX-20260216`; command pack in section `22.9`. |

### 22.6 Expected-loss model for release-gate decisions

State set:
- `P0`: all required lanes pass with reproducible evidence,
- `P1`: deterministic mismatches with complete evidence,
- `P2`: missing evidence in one or more required lanes,
- `P3`: contradictory or stale risk/perf/test claims.

Action set:
- `Q0`: approve lane and advance,
- `Q1`: block lane and file/update blocker bead,
- `Q2`: rerun targeted strict/hardened replays with fixed seed,
- `Q3`: fail closed and hold release candidate.

Loss matrix (lower is better):

| State \ Action | `Q0` | `Q1` | `Q2` | `Q3` |
|---|---:|---:|---:|---:|
| `P0` full evidence | 0 | 2 | 3 | 5 |
| `P1` mismatch with evidence | 7 | 1 | 2 | 2 |
| `P2` missing evidence | 10 | 4 | 3 | 1 |
| `P3` contradictory claims | 10 | 5 | 4 | 1 |

Calibration target:
- gate classification reproducibility `>= 0.9` across reruns with identical seed and inputs.
Fallback trigger:
- any `P2`/`P3` result forces `Q3` (NO-GO) until evidence/contradiction is resolved.

### 22.7 One-lever optimization loop (risk/perf/test specialist)

Single lever selected for next cycle:
- codify a deterministic flake-budget classifier over `suite_status.tsv` + per-suite JSON reports from the live orchestrator bundle.

Required artifacts:
1. baseline flake/mismatch profile from existing bundles,
2. classifier implementation and threshold rationale,
3. strict/hardened rerun validation,
4. isomorphism proof that classifier does not mask true parity drift,
5. post-change delta report with confidence bounds.

Dependency targets:
- `bd-2wb.19.5` (packet-008 unit/property evidence),
- `bd-2wb.19.6` (packet-008 differential/adversarial evidence),
- `bd-2wb.19.7` (packet-008 E2E journey evidence),
- `bd-2wb.19.8` and `bd-2wb.19.9` (packet-008 optimization + final evidence pack).

### 22.8 Go / No-Go criteria for `bd-2wb.24.14` handoff

Go requires all conditions:
1. contradiction registers from sections `20` and `25` remain bounded with no unresolved untracked item;
2. behavior deep-pass matrices (`21`, `26`) remain evidence-backed and current;
3. gate readiness matrix in section `22.4` has explicit blocker ownership for every non-ready gate.

No-Go triggers:
1. any high-risk lane without replay command or forensic-field contract,
2. any risk/perf/test claim not tied to source/test/e2e/log evidence,
3. any silent downgrade of strict/hardened parity boundary.

### 22.9 Specialist acceptance evidence commands

- `UBS_MAX_DIR_SIZE_MB=5000 ubs --diff`
- `rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_009_u013_strict_mode_rejects_unsafe_tls_config_and_records_event`
- `rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract`
- `rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u003_handshake_requires_ping_first`
- `rch exec -- cargo test -p fr-conformance -- --nocapture conformance_protocol_fixture_passes`
- `rch exec -- cargo check -p fr-conformance --all-targets`
