# PLAN_TO_PORT_REDIS_TO_RUST

## 1. Porting Method (Spec-First)

1. Extract behavior from `legacy_redis_code/redis` into stable specs.
2. Implement from specs, not line-by-line translation.
3. Validate via differential conformance fixtures.
4. Optimize only with profile + isomorphism proof artifacts.

## 2. Current Phase

- Phase 1 complete: bootstrap docs and workspace skeleton.
- Phase 2 in progress: extraction docs exist; deep subsystem extraction still incomplete.
- Phase 3 in progress: architecture synthesized; strict/hardened mode split defined.
- Phase 4 started: first executable vertical slice landed (`RESP -> router -> store -> runtime -> conformance`).
- Phase 5 started: fixture-driven conformance and baseline artifacts are wired.

## 3. V1 Scope (Committed)

- RESP2/RESP3 framing and command dispatch skeleton.
- Core string/key operations and TTL path.
- AOF/RDB scaffolding with replay-shape contract.
- Baseline primary/replica state progression scaffolding.
- Strict vs hardened compatibility gates and evidence ledger surface.

## 4. Explicit V1 Exclusions (Current)

- Sentinel and module ecosystems.
- Full Redis cluster behavior parity.
- TLS/IO-thread production complexity.
- Lua/module execution compatibility.
- Long-tail command families outside V1 matrix.

## 5. Milestone Sequence

### M1 - Extraction Closure

- Finish `EXISTING_REDIS_STRUCTURE.md` with concrete function-level behavior for:
  - protocol parser
  - command table and arity rules
  - TTL semantics edge cases
  - AOF/RDB replay ordering
  - replication offsets and partial resync

Exit criteria:
- Extraction packet complete for all V1 families.
- No unresolved ambiguity on return values, ordering, and error strings.

### M2 - Core Semantic Parity

- Expand runtime command set from bootstrap to V1 matrix.
- Add fixture families per command cluster.
- Add drift taxonomy and severity gates.

Exit criteria:
- No critical drifts for V1 command families.

### M3 - Persistence + Replication Parity

- Implement AOF/RDB replay ordering invariants.
- Implement first-wave replication offsets, backlog, and handshake parity.

Exit criteria:
- Persistence and replication parity suites pass in strict mode.

### M4 - Hardening + Performance

- Harden parser/state transitions for hostile inputs.
- Add profile-driven optimizations one lever at a time.
- Keep strict-mode behavior isomorphic.

Exit criteria:
- Security suite green.
- Performance budgets and no parity regression.

### M5 - Release Readiness

- Full conformance runbook and report export.
- RaptorQ sidecar generation for persistent artifacts.
- Final compatibility matrix and documented exceptions.

Exit criteria:
- Gate A/B/C/D in `COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md` pass.

## 6. Required Evidence Per Meaningful Change

1. Differential conformance report.
2. Invariant checklist update.
3. Benchmark delta report (or explicit defer note).
4. Risk-note update if compatibility/security surface changed.
5. Isomorphism proof note for optimization changes.

## 7. Immediate Next Batch (Near-Term)

1. Add RESP parser negative corpus fixtures.
2. Add Redis-compatible error-string normalization for implemented commands.
3. Add deterministic persistence replay fixture family.
4. Add first replication handshake fixture family.
5. Wire RaptorQ sidecar manifest generation into conformance output pipeline.
