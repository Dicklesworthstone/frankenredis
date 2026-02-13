# COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1

## 0. Prime Directive

Build a system that is simultaneously:

1. Behaviorally trustworthy for scoped compatibility.
2. Mathematically explicit in decision and risk handling.
3. Operationally resilient via RaptorQ-backed durability.
4. Performance-competitive via profile-and-proof discipline.

Crown-jewel innovation:

Deterministic Latency Replication Core (DLRC): strict command semantics with tail-aware scheduling and recoverable persistence pipelines.

Legacy oracle:

- /dp/frankenredis/legacy_redis_code/redis
- upstream: https://github.com/redis/redis

Exemplar reference:

- `/data/projects/frankenredis/COMPREHENSIVE_SPEC_FOR_FRANKENSQLITE_V1_REFERENCE.md` (copied from `/dp/frankensqlite`).

## 1. Product Thesis

Most reimplementations fail by being partially compatible and operationally brittle. FrankenRedis will instead combine compatibility realism with first-principles architecture and strict quality gates.

## 2. V1 Scope Contract

Included in V1:

- RESP2/RESP3 core paths
- scoped key and data-type command families
- TTL and persistence scope
- primary and replica basics

Deferred from V1:

- long-tail API surface outside highest-value use cases
- broad ecosystem parity not required for core migration value
- distributed/platform expansion not needed for V1 acceptance

## 3. Architecture Blueprint

RESP parser -> command router -> data engine -> persistence -> replication

Planned crate families:
- fr-eventloop
- fr-protocol
- fr-command
- fr-store
- fr-expire
- fr-persist
- fr-repl
- fr-config
- fr-runtime
- fr-conformance
- frankenredis (integration binary; pending)

## 4. Compatibility Model (frankenlibc/frankenfs-inspired)

Two explicit operating modes:

1. strict mode:
   - maximize observable compatibility for scoped APIs
   - no behavior-altering repair heuristics
2. hardened mode:
   - maintain outward contract while enabling defensive runtime checks and bounded repairs

Compatibility focus for this project:

Preserve Redis-observable replies, side effects, and ordering guarantees for scoped command sets.

Fail-closed policy:

- unknown incompatible features or protocol fields must fail closed by default
- compatibility exceptions require explicit allowlist entries and audit traces

## 5. Security Model

Security focus for this project:

Defend against malformed protocol frames, replay/order attacks, and persistence tampering.

Threat model baseline:

1. malformed input and parser abuse
2. state-machine desynchronization
3. downgrade and compatibility confusion paths
4. persistence corruption and replay tampering

Mandatory controls:

- adversarial fixtures and fuzz/property suites for high-risk entry points
- deterministic audit trail for recoveries and mode/policy overrides
- explicit subsystem ownership and trust-boundary notes

## 6. Alien-Artifact Decision Layer

Runtime controllers (scheduling, adaptation, fallback, admission) must document:

1. state space
2. evidence signals
3. loss matrix with asymmetric costs
4. posterior or confidence update model
5. action rule minimizing expected loss
6. calibration fallback trigger

Output requirements:

- evidence ledger entries for consequential decisions
- calibrated confidence metrics and drift alarms

## 7. Extreme Optimization Contract

Track throughput and p95/p99 latency under mixed workloads; gate persistence overhead and replication-lag regressions.

Optimization loop is mandatory:

1. baseline metrics
2. hotspot profile
3. single-lever optimization
4. behavior-isomorphism proof
5. re-profile and compare

No optimization is accepted without associated correctness evidence.

## 8. Correctness and Conformance Contract

Maintain deterministic command semantics, expiration behavior, and AOF/RDB recovery ordering invariants.

Conformance process:

1. generate canonical fixture corpus
2. run legacy oracle and capture normalized outputs
3. run FrankenRedis and compare under explicit equality/tolerance policy
4. produce machine-readable parity report artifact

Assurance ladder:

- Tier A: unit/integration/golden fixtures
- Tier B: differential conformance
- Tier C: property/fuzz/adversarial tests
- Tier D: regression corpus for historical failures

## 9. RaptorQ-Everywhere Durability Contract

RaptorQ repair-symbol sidecars are required for long-lived project evidence:

1. conformance snapshots
2. benchmark baselines
3. migration manifests
4. reproducibility ledgers
5. release-grade state artifacts

Required artifacts:

- symbol generation manifest
- scrub verification report
- decode proof for each recovery event

## 10. Milestones and Exit Criteria

### M0 — Bootstrap

- workspace skeleton
- CI and quality gate wiring

Exit:
- fmt/check/clippy/test baseline green

### M1 — Core Model

- core data/runtime structures
- first invariant suite

Exit:
- invariant suite green
- first conformance fixtures passing

### M2 — First Vertical Slice

- end-to-end scoped workflow implemented

Exit:
- differential parity for first major API family
- baseline benchmark report published

### M3 — Scope Expansion

- additional V1 API families

Exit:
- expanded parity reports green
- no unresolved critical compatibility defects

### M4 — Hardening

- adversarial coverage and perf hardening

Exit:
- regression gates stable
- conformance drift zero for V1 scope

## 11. Acceptance Gates

Gate A: compatibility parity report passes for V1 scope.

Gate B: security/fuzz/adversarial suite passes for high-risk paths.

Gate C: performance budgets pass with no semantic regressions.

Gate D: RaptorQ durability artifacts validated and scrub-clean.

All four gates must pass for V1 release readiness.

## 12. Risk Register

Primary risk focus:

Tail-latency and recovery-order regressions under concurrent stress conditions.

Mitigations:

1. compatibility-first development for risky API families
2. explicit invariants and adversarial tests
3. profile-driven optimization with proof artifacts
4. strict mode/hardened mode separation with audited policy transitions
5. RaptorQ-backed resilience for critical persistent artifacts

## 13. Immediate Execution Checklist

1. Create workspace and crate skeleton.
2. Implement smallest high-value end-to-end path in V1 scope.
3. Stand up differential conformance harness against legacy oracle.
4. Add benchmark baseline generation and regression gating.
5. Add RaptorQ sidecar pipeline for conformance and benchmark artifacts.

## 14. Detailed Crate Contracts (V1)

| Crate | Primary Responsibility | Explicit Non-Goal | Invariants | Mandatory Tests |
|---|---|---|---|---|
| fr-types | core protocol/value/state type model | network IO | stable value encoding tags; deterministic ordering contracts | type matrix + serialization round-trip |
| fr-resp | RESP2/RESP3 parsing + encoding | command execution | frame boundaries and error classes deterministic | protocol corpus + malformed frame tests |
| fr-command | command registry + dispatch routing | persistence internals | command flag semantics and arity checks preserved | command matrix fixtures |
| fr-store | in-memory key/value engine and expiry metadata | replication transport | mutation ordering and TTL invariants preserved | mutation+TTL property tests |
| fr-aof | append-only persistence log handling | snapshot format design | log replay ordering fidelity | AOF replay parity tests |
| fr-rdb | snapshot read/write and restore flow | protocol parsing | snapshot schema consistency and deterministic decode | RDB round-trip fixtures |
| fr-repl | primary/replica sync state machine | cluster slot balancing | replication phase transitions deterministic | replication integration fixtures |
| fr-net | socket/eventloop integration and admission checks | command semantics | deterministic request lifecycle and bounded admission policy | eventloop + backpressure tests |
| fr-conformance | differential harness vs Redis legacy oracle | production serving | explicit comparison policy by command family | report schema + differential runner tests |
| frankenredis | integration binary/library and policy loading | algorithm design | strict/hardened mode wiring and evidence logging | mode gate and startup tests |

## 15. Conformance Matrix (V1)

| Family | Oracle Workload | Pass Criterion | Drift Severity |
|---|---|---|---|
| RESP framing + parse | protocol corpus with mixed frame classes | exact parse/reply parity | critical |
| core command semantics | SET/GET/DEL/INCR and scoped hash/list/set cases | reply + side-effect parity | critical |
| TTL + expiration | mixed expire/ttl workloads | expiration-time and visibility parity | critical |
| persistence replay | AOF/RDB load + replay fixtures | restored state parity | critical |
| replication first wave | primary/replica sync workloads | lag + state parity under policy | high |
| ACL/auth scoped behavior | auth and rule fixtures | allow/deny parity and audit parity | high |
| config + protocol toggles | config mutation fixtures | deterministic config semantics | high |
| mixed E2E pipeline | protocol -> commands -> persistence -> replay | reproducible parity report with no critical drift | critical |

## 16. Security and Compatibility Threat Matrix

| Threat | Strict Mode Response | Hardened Mode Response | Required Artifact |
|---|---|---|---|
| malformed RESP frame abuse | fail-closed parse error | fail-closed + bounded diagnostics | protocol incident ledger |
| ACL confusion or bypass attempt | reject unauthorized command | reject + explicit policy audit | ACL decision ledger |
| command amplification abuse | execute scoped semantics as specified | admission controls + explicit reject path | admission decision log |
| persistence tampering | fail load on invalid artifacts | recover only with validated sidecar proof | decode proof + tamper ledger |
| replication replay mismatch | fail replication step | fail + quarantine replica state | replication incident report |
| unknown incompatible config field | fail-closed | fail-closed | compatibility drift report |
| oracle mismatch in conformance | hard fail | hard fail | conformance failure bundle |
| override misuse | explicit override + audit trail | explicit override + audit trail | override audit record |

## 17. Performance Budgets and SLO Targets

| Path | Workload Class | Budget |
|---|---|---|
| RESP parse hot path | mixed 128B-4KB frames | p95 <= 120 us |
| command dispatch + store | scoped mixed key workloads | p95 <= 1.5 ms |
| single-node throughput | mixed read/write scoped command set | >= 150k ops/s |
| expiration sweep overhead | TTL-heavy workload | p95 regression <= +8% |
| AOF append overhead | write-heavy workload | p95 regression <= +12% |
| replication catch-up | scoped primary/replica benchmark | lag p95 <= 250 ms |
| memory footprint | mixed E2E workload | peak RSS regression <= +10% |
| tail stability | all benchmark families | p99 regression <= +10% |

Optimization acceptance rule:
1. primary metric improves or remains within budget,
2. no critical conformance drift,
3. p99 and memory budgets remain within limits.

## 18. CI Gate Topology (Release-Critical)

| Gate | Name | Blocking | Output Artifact |
|---|---|---|---|
| G1 | format + lint | yes | lint report |
| G2 | unit + integration | yes | junit report |
| G3 | differential conformance | yes | parity report JSON + markdown summary |
| G4 | adversarial + property tests | yes | minimized counterexample corpus |
| G5 | benchmark regression | yes | baseline delta report |
| G6 | RaptorQ scrub + recovery drill | yes | scrub report + decode proof sample |

Release cannot proceed unless all gates pass on the same commit.

## 19. RaptorQ Artifact Envelope (Project-Wide)

Persistent evidence artifacts must be emitted with sidecars:
1. source artifact hash manifest,
2. RaptorQ symbol manifest,
3. scrub status,
4. decode proof log when recovery occurs.

Canonical envelope schema:

~~~json
{
  "artifact_id": "string",
  "artifact_type": "conformance|benchmark|ledger|manifest",
  "source_hash": "blake3:...",
  "raptorq": {
    "k": 0,
    "repair_symbols": 0,
    "overhead_ratio": 0.0,
    "symbol_hashes": ["..."]
  },
  "scrub": {
    "last_ok_unix_ms": 0,
    "status": "ok|recovered|failed"
  },
  "decode_proofs": [
    {
      "ts_unix_ms": 0,
      "reason": "...",
      "recovered_blocks": 0,
      "proof_hash": "blake3:..."
    }
  ]
}
~~~

## 20. 90-Day Execution Plan

Weeks 1-2:
- scaffold workspace and crate boundaries
- lock protocol + command conformance schema

Weeks 3-5:
- implement fr-types/fr-resp/fr-command/fr-store minimal vertical slice
- land first strict-mode differential conformance run

Weeks 6-8:
- implement AOF/RDB + replication first wave
- publish baseline benchmarks against section-17 budgets

Weeks 9-10:
- harden ACL/config/parser boundary and adversarial corpus
- wire strict/hardened policy transitions with audit traces

Weeks 11-12:
- enforce full gate topology G1-G6 in CI
- run release-candidate drill with full artifact envelope

## 21. Porting Artifact Index

This spec is paired with the following methodology artifacts:

1. PLAN_TO_PORT_REDIS_TO_RUST.md
2. EXISTING_REDIS_STRUCTURE.md
3. PROPOSED_ARCHITECTURE.md
4. FEATURE_PARITY.md

Rule of use:

- Extraction and behavior understanding happens in EXISTING_REDIS_STRUCTURE.md.
- Scope, exclusions, and phase sequencing live in PLAN_TO_PORT_REDIS_TO_RUST.md.
- Rust crate boundaries live in PROPOSED_ARCHITECTURE.md.
- Delivery readiness is tracked in FEATURE_PARITY.md.

## 22. Current Implementation Snapshot (2026-02-13)

Completed bootstrap vertical slice:

- `fr-protocol`: RESP frame model + parser/encoder.
- `fr-command`: deterministic bootstrap command set (`PING`, `ECHO`, `SET`, `GET`, `DEL`, `INCR`, `EXPIRE`, `PTTL`).
- `fr-store`: key/value + TTL behavior with Redis-compatible `PTTL` sentinel values.
- `fr-runtime`: strict-mode compatibility gate and evidence ledger emission surface.
- `fr-conformance`: fixture-driven differential harness with stateful sequence execution.
- `fr-persist`: AOF record frame mapping scaffold.
- `fr-repl`: replication state/offset scaffold.

Artifacts produced in this iteration:

- `baselines/round1_conformance_baseline.json`
- `baselines/round1_conformance_strace.txt`
- `golden_outputs/core_strings.json`
- `golden_checksums.txt`

Extended conformance coverage in this batch:

- Redis-style command error normalization for unknown command, arity, syntax, integer, and overflow paths.
- Protocol-negative fixture family for malformed RESP and fail-closed parse behavior.
- Persistence replay fixture family using AOF-shaped records and post-replay assertions.

## 23. Asupersync + FrankenTUI Adoption Contract

### Asupersync adoption path

1. Replace placeholder eventloop scheduling with Asupersync-backed runtime adapter.
2. Use structured-cancellation and tracing hooks for deterministic replication pipeline control.
3. Emit runtime traces as durable artifacts eligible for RaptorQ sidecars.

### FrankenTUI adoption path

1. Build operator dashboard for conformance drift and replication lag.
2. Render galaxy-brain evidence cards from runtime decision ledger.
3. Add strict/hardened mode transition telemetry panel with deterministic audit stream.

## 24. Alien-Graveyard Opportunity Matrix (Round 1)

| Candidate Lever | Impact | Confidence | Effort | Score | Decision |
|---|---:|---:|---:|---:|---|
| Adaptive Radix Tree for keyspace indexing | 5 | 3 | 3 | 5.0 | queue for post-baseline implementation |
| Flat Combining for command-queue contention | 4 | 3 | 3 | 4.0 | queue after multi-client benchmark exists |
| S3-FIFO eviction policy for memory pressure | 4 | 4 | 2 | 8.0 | prioritize once eviction path lands |
| RaptorQ sidecars for conformance/benchmark artifacts | 5 | 4 | 2 | 10.0 | prioritized contract, implementation pending |
| e-values/conformal calibration for adaptive controller | 3 | 3 | 3 | 3.0 | add after first adaptive controller exists |

All scores satisfy threshold (`>= 2.0`), but only one lever is permitted per optimization change.
