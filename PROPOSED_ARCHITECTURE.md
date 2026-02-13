# PROPOSED_ARCHITECTURE

## 1. Non-Negotiable Principles

1. Spec-first implementation; no line-by-line legacy translation.
2. Dual-mode operation: strict compatibility and hardened safety.
3. Deterministic semantics first; optimization second.
4. Every meaningful change emits proof, perf, and risk artifacts.
5. Persistent artifacts are RaptorQ-sidecar eligible by contract.

## 2. Execution Pipeline

`RESP decode -> command routing -> store mutation/read -> persistence hooks -> replication hooks -> reply encode`

The current codebase already has a first executable vertical slice for this path.

## 3. Workspace Crate Responsibilities

- `fr-protocol`: RESP frame types, parser, encoder, and parse error model.
- `fr-command`: arity/option validation and command execution mapping.
- `fr-store`: in-memory keyspace core and TTL-aware semantics.
- `fr-runtime`: strict/hardened gatekeeping and evidence ledger hooks.
- `fr-conformance`: fixture runner and differential-report shape.
- `fr-persist`: AOF record shape and replay-frame contract scaffolding.
- `fr-repl`: replication state and offset progression scaffolding.
- `fr-expire`: expiry decision primitives and return-code policy.
- `fr-eventloop`: tick-budget and backlog scheduling primitives.
- `fr-config`: mode and compatibility gate policy model.

## 4. Strict vs Hardened Mode Contract

### Strict mode

- Maximize Redis-observable compatibility.
- No behavior-altering recovery heuristics.
- Fail closed on unknown incompatible surfaces.

### Hardened mode

- Preserve outward API contract.
- Add bounded defensive checks and incident evidence records.
- Continue fail-closed for unknown incompatible surfaces.

## 5. Asupersync + FrankenTUI Integration Plan

### Asupersync

- Runtime execution backend for network/eventloop scheduling.
- Deterministic task orchestration and trace-capable concurrency primitives.
- Adapter surface lives in `fr-runtime::ecosystem::AsyncRuntimeAdapter`.

### FrankenTUI

- Operator and evidence rendering surface (drift, gate events, policy overrides).
- Galaxy-brain math/explainability cards for runtime decisions.
- Adapter surface lives in `fr-runtime::ecosystem::OperatorUiAdapter`.

## 6. Data and Control Invariants

1. Command side effects are ordered and deterministic for equal inputs.
2. TTL return-code behavior matches Redis contract (`PTTL`: `-2`, `-1`, positive).
3. Compatibility gates fail closed with explicit evidence events.
4. Replication offset monotonicity is maintained by state model.
5. Persistence record frames remain reversible (`AofRecord <-> RESP frame`).

## 7. Conformance Architecture

1. Fixture format: deterministic case list with command argv, timestamp, expected frame.
2. Runner: shared runtime instance for stateful sequence semantics.
3. Report: total/passed/failed with expected/actual frame capture.
4. Drift classification: `critical`, `high`, `medium`, `low` (spec-level taxonomy).

## 8. Performance and Optimization Contract

1. Baseline benchmark and profile capture before optimization.
2. Opportunity matrix scoring (`impact * confidence / effort`).
3. One optimization lever per change.
4. Isomorphism proof and golden output checksum verification.
5. Re-baseline and compare p50/p95/p99 + memory + syscall distribution.

## 9. Security and Durability Contract

- Protocol and compatibility mismatches are fail-closed.
- Decision incidents are evidence-ledger visible.
- Long-lived conformance/benchmark artifacts are sidecar-ready for RaptorQ envelope.
- Recovery/decode proof integration is mandatory for future persistence layers.
