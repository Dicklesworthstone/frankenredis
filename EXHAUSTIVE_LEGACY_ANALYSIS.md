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
