# FrankenRedis

FrankenRedis is a clean-room Rust reimplementation targeting grand-scope excellence: semantic fidelity, mathematical rigor, operational safety, and profile-proven performance.

## What Makes This Project Special

Deterministic Latency Replication Core (DLRC): strict command semantics with tail-aware scheduling and recoverable persistence pipelines.

This is treated as a core identity constraint, not a best-effort nice-to-have.

## Methodological DNA

This project uses four pervasive disciplines:

1. alien-artifact-coding for decision theory, confidence calibration, and explainability.
2. extreme-software-optimization for profile-first, proof-backed performance work.
3. RaptorQ-everywhere for self-healing durability of long-lived artifacts and state.
4. frankenlibc/frankenfs compatibility-security thinking: strict vs hardened mode separation, fail-closed compatibility gates, and explicit drift ledgers.

## Current State

- project charter and porting docs established
- legacy oracle cloned at `/data/projects/frankenredis/legacy_redis_code/redis`
- first executable vertical slice landed:
  - RESP parser/encoder
  - bootstrap command router (`PING`, `ECHO`, `SET`, `GET`, `DEL`, `INCR`, `EXPIRE`, `PTTL`)
  - in-memory store + TTL semantics
  - strict compatibility gate + evidence ledger scaffold
  - fixture-driven conformance harness (`core_strings`, `core_errors`, `protocol_negative`, `persist_replay`)
- baseline and proof artifacts added:
  - `baselines/round1_conformance_baseline.json`
  - `baselines/round1_conformance_strace.txt`
  - `baselines/round2_protocol_negative_baseline.json`
  - `baselines/round2_protocol_negative_strace.txt`
  - `golden_checksums.txt`

## V1 Scope

- RESP2/RESP3 core paths
- scoped key and data-type command families
- TTL and persistence scope
- primary and replica basics

## Architecture Direction

RESP parser -> command router -> data engine -> persistence -> replication

## Compatibility and Security Stance

Preserve Redis-observable replies, side effects, and ordering guarantees for scoped command sets.

Defend against malformed protocol frames, replay/order attacks, and persistence tampering.

## Performance and Correctness Bar

Track throughput and p95/p99 latency under mixed workloads; gate persistence overhead and replication-lag regressions.

Maintain deterministic command semantics, expiration behavior, and AOF/RDB recovery ordering invariants.

## Key Documents

- AGENTS.md
- COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md
- COMPREHENSIVE_SPEC_FOR_FRANKENSQLITE_V1_REFERENCE.md (copied exemplar from `frankensqlite`)

## Next Steps

1. Expand conformance fixtures to parser-negative corpus and strict Redis error-string parity.
2. Land persistence replay invariants and replication handshake fixtures.
3. Add Asupersync-backed runtime adapter and FrankenTUI operator dashboard adapter.
4. Implement first RaptorQ sidecar pipeline for baseline/conformance artifacts.
5. Run optimization loop with one lever per commit and isomorphism proofs.

## Porting Artifact Set

- PLAN_TO_PORT_REDIS_TO_RUST.md
- EXISTING_REDIS_STRUCTURE.md
- PROPOSED_ARCHITECTURE.md
- FEATURE_PARITY.md

These four docs are now the canonical porting-to-rust workflow for this repo.

## Validation Commands

```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cargo test -p fr-conformance -- --nocapture
cargo bench
```

## Round 1 Benchmark Script

```bash
./scripts/benchmark_round1.sh
```

## Round 2 Benchmark Script

```bash
./scripts/benchmark_round2.sh
```
