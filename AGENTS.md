# AGENTS.md — FrankenRedis

Guidelines for AI coding agents working in this Rust codebase.

---

## RULE 0 - THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 1: NO FILE DELETION

YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.

YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.

---

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

1. Forbidden without explicit authorization: git reset --hard, git clean -fd, rm -rf, or any irreversible overwrite/delete command.
2. If command impact is uncertain, stop and ask.
3. Prefer non-destructive alternatives first.
4. Restate approved destructive command and impacted paths before execution.
5. Log user authorization text, command, and execution timestamp.

---

## Git Branch Policy

- Default branch is main.
- Do not introduce master references in code/docs/CI.
- If requested, keep legacy master synced from main.

---

## Toolchain Policy

- Cargo-only workflow.
- Rust 2024 edition.
- Explicit dependency versions.
- Prefer forbid unsafe code by default.
- If narrow unsafe usage is unavoidable, isolate it behind audited interfaces and tests.

---

## Mandatory Method Stack (Non-Negotiable)

Every meaningful implementation decision must apply all four methods:

1. alien-artifact-coding:
   - decision-theoretic runtime contracts
   - evidence ledgers
   - formal safety/calibration claims
2. extreme-software-optimization:
   - profile-first optimization
   - one optimization lever per change
   - behavior-isomorphism proof artifacts
3. RaptorQ-everywhere durability:
   - durable artifacts have repair-symbol sidecars
   - decode proofs for any recovery path
   - background integrity scrub requirements
4. frankenlibc/frankenfs security-compatibility doctrine:
   - strict compatibility mode + hardened mode separation
   - fail-closed on unknown incompatible features
   - explicit compatibility matrix and drift gates

---

## FrankenRedis — Project Identity

Crown-jewel innovation:

Deterministic Latency Replication Core (DLRC): strict command semantics with tail-aware scheduling and recoverable persistence pipelines.

Legacy behavioral oracle:

- /dp/frankenredis/legacy_redis_code/redis
- upstream: https://github.com/redis/redis

CRITICAL NON-REGRESSION RULE:

Redis command semantics and replication ordering are core contracts for V1 scope.

---

## Architecture (Target)

RESP parser -> command router -> data engine -> persistence -> replication

Planned workspace crates:
- fr-types
- fr-resp
- fr-command
- fr-store
- fr-aof
- fr-rdb
- fr-repl
- fr-net
- fr-conformance
- frankenredis

---

## Compatibility Doctrine (Mode-Split)

- strict mode:
  - maximize observable compatibility for V1 scoped APIs
  - no behavior-altering repairs
- hardened mode:
  - preserve API contract while adding safety guards
  - bounded defensive recovery for malformed inputs and hostile edge cases

Compatibility focus for this project:

Preserve Redis-observable replies, side effects, and ordering guarantees for scoped command sets.

---

## Security Doctrine

Security focus for this project:

Defend against malformed protocol frames, replay/order attacks, and persistence tampering.

Minimum security bar:

1. Threat model notes for each major subsystem.
2. Fail-closed behavior for unknown incompatible features.
3. Adversarial fixture coverage and fuzz/property tests for high-risk parsers/state transitions.
4. Deterministic audit logs for recoveries and policy overrides.

---

## RaptorQ-Everywhere Contract

RaptorQ sidecar durability applies to:

- conformance fixture bundles
- benchmark baseline bundles
- migration manifests
- reproducibility ledgers
- long-lived state snapshots

Required outputs:

1. Repair-symbol generation manifest.
2. Integrity scrub report.
3. Decode proof artifact for each recovery event.

---

## Performance Doctrine

Track throughput and p95/p99 latency under mixed workloads; gate persistence overhead and replication-lag regressions.

Mandatory optimization loop:

1. Baseline: record p50/p95/p99 and memory.
2. Profile: identify real hotspots.
3. Implement one optimization lever.
4. Prove behavior unchanged via conformance + invariant checks.
5. Re-baseline and emit delta artifact.

---

## Correctness Doctrine

Maintain deterministic command semantics, expiration behavior, and AOF/RDB recovery ordering invariants.

Required evidence for substantive changes:

- differential conformance report
- invariant checklist update
- benchmark delta report
- risk-note update if threat or compatibility surface changed

---

## Required Check Commands (Post-Change)

~~~bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --workspace
~~~

If conformance/bench crates exist, also run:

~~~bash
cargo test -p conformance -- --nocapture
cargo bench
~~~

---

## Landing The Plane

Before ending a meaningful work session:

1. Confirm no destructive operations were run without explicit permission.
2. Summarize changes and rationale.
3. List residual risks and next highest-value steps.
4. Confirm method-stack artifacts were produced or explicitly deferred.
