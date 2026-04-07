# FrankenRedis

<div align="center">
  <img src="frankenredis_illustration.webp" alt="FrankenRedis - memory-safe clean-room Redis reimplementation in Rust">
</div>

FrankenRedis is a clean-room Rust reimplementation targeting grand-scope excellence: semantic fidelity, operational safety, and performance-focused design.

Absolute project goal: full drop-in replacement parity with legacy Redis behavior for the complete intended Redis surface, not a permanently reduced feature subset.

## What Makes This Project Special

Deterministic Latency Replication Core (DLRC): strict command semantics with tail-aware scheduling and recoverable persistence pipelines.

This is treated as a core identity constraint, not a best-effort nice-to-have.

## Methodological DNA

This project applies frankenlibc/frankenfs compatibility-security thinking: strict vs hardened mode separation, fail-closed compatibility gates, and explicit drift ledgers. This is implemented and enforced throughout the codebase via `fr-config::Mode` and the runtime policy system.

## Roadmap

The following disciplines are planned but not yet integrated:

1. **alien-artifact-coding** for decision theory, confidence calibration, and explainability. Status: not started.
2. **extreme-software-optimization** for profile-first, proof-backed performance work. Status: optimization proof artifacts exist (`ISOMORPHISM_PROOF_ROUND{1,2}.md`), but workload benchmarks against live servers are pending.
3. **RaptorQ-everywhere** for self-healing durability of long-lived artifacts and state. Status: not started, no crate dependency added.

## Current State

- project charter and porting docs established
- legacy oracle cloned at `/data/projects/frankenredis/legacy_redis_code/redis`
- first executable vertical slice landed:
  - standalone `fr-server` binary (`frankenredis`) using `mio` for single-threaded TCP serving on top of `fr-runtime`
  - RESP parser/encoder
  - broad command surface across strings, hashes, lists, sets, sorted sets, streams, geo, pub/sub, and server control paths
  - in-memory store + TTL semantics
  - replication sync baseline: `PSYNC`/`SYNC` negotiation, full-resync snapshot apply, partial backlog replay, replica reconnect flow, and live replication offset reporting
  - strict/hardened compatibility gate + runtime evidence/structured-log baseline
  - fixture-driven conformance harness (`core_*` families + phase2c packet suites)
- checked-in optimization and proof artifacts currently include:
  - `artifacts/optimization/phase2c-gate/baseline_hyperfine.json`
  - `artifacts/optimization/phase2c-gate/baseline_strace.txt`
  - `artifacts/optimization/phase2c-gate/after_hyperfine_multi.json`
  - `artifacts/optimization/phase2c-gate/after_multi_strace.txt`
  - `artifacts/optimization/ISOMORPHISM_PROOF_ROUND1.md`
  - `artifacts/optimization/ISOMORPHISM_PROOF_ROUND2.md`
  - `artifacts/phase2c/schema/topology_lock_v1.json`

## Full Drop-In Parity Contract

- 100% feature/functionality overlap with legacy Redis target surface is mandatory.
- Any staged rollout is sequencing only, never a permanent exclusion.
- Every deferred surface must be represented as an explicit blocking backlog item with closure criteria.
- Strict mode must preserve Redis-observable replies, side effects, and ordering across the full parity program.

## Architecture Direction

tcp client -> fr-server -> fr-runtime -> RESP parser -> command router -> data engine -> persistence -> replication

## Concrete Execution Path (Current Code)

1. Network ingress starts in the standalone `frankenredis` binary at `crates/fr-server/src/main.rs`, which runs a single-threaded `mio` event loop, owns TCP connection state, and delegates per-command execution to `fr-runtime`.
2. Runtime ingress inside that server path reaches `Runtime::execute_bytes` (`crates/fr-runtime/src/lib.rs`), which parses wire bytes and emits fail-closed evidence on protocol errors.
3. `Runtime::execute_frame` performs preflight policy checks, handles special runtime commands (auth, acl, config, cluster, transaction, persistence controls), enforces auth/maxmemory gates, and runs active-expire before general dispatch.
4. General command dispatch flows through `fr_command::dispatch_argv` (`crates/fr-command/src/lib.rs`) into command handlers that mutate/read `Store` with deterministic `now_ms` semantics.
5. Expiration semantics are centralized in store + `fr-expire` (`evaluate_expiry`), preserving Redis-visible `TTL/PTTL` return contracts (`-2`, `-1`, positive remaining lifetime).
6. Successful write dispatch captures persistence/replication signals in runtime (`capture_aof_record`), appending `fr-persist::AofRecord` entries and advancing replication offsets.
7. Replication state-machine logic lives in `fr-repl`, while the live server path in `fr-server` handles replica sockets, backlog delivery, and reconnect flow on top of runtime state.
8. Conformance execution is driven by `fr-conformance::run_fixture`, which instantiates strict or hardened runtime modes and validates both reply parity and threat/evidence expectations.

## Compatibility and Security Stance

Preserve Redis-observable replies, side effects, and ordering guarantees for full parity scope.

Defend against malformed protocol frames, replay/order attacks, and persistence tampering.

## Performance and Correctness Bar

Track throughput and p95/p99 latency under mixed workloads; gate persistence overhead and replication-lag regressions.

Maintain deterministic command semantics, expiration behavior, and AOF/RDB recovery ordering invariants.

## Key Documents

- AGENTS.md
- COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md
- COMPREHENSIVE_SPEC_FOR_FRANKENSQLITE_V1_REFERENCE.md (copied exemplar from `frankensqlite`)
- TEST_LOG_SCHEMA_V1.md

## Next Steps

1. Expand conformance fixtures until all command families and compatibility-critical behaviors are covered.
2. Expand persistence and replication invariants from the implemented sync baseline to broader legacy-oracle parity coverage.
3. Add Asupersync-backed runtime adapter and FrankenTUI operator dashboard adapter.
4. Implement RaptorQ sidecar pipeline for all durability-critical artifacts.
5. Run optimization loop with one lever per commit and isomorphism proofs while preserving strict parity.

## Porting Artifact Set

- PLAN_TO_PORT_REDIS_TO_RUST.md
- EXISTING_REDIS_STRUCTURE.md
- PROPOSED_ARCHITECTURE.md
- FEATURE_PARITY.md

These four docs are now the canonical porting-to-rust workflow for this repo.

## Validation Commands

```bash
# Offloaded (recommended in multi-agent sessions)
rch exec -- cargo fmt --check
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
rch exec -- cargo test --workspace
rch exec -- cargo test -p fr-conformance -- --nocapture
rch exec -- cargo run -p fr-conformance --bin phase2c_schema_gate -- --optimization-gate
rch exec -- cargo bench

# The benchmark gate runs locally and offloads only its release build step via rch.
./scripts/benchmark_gate.sh

# If rch is unavailable, run the same commands with plain cargo.
```

## License

MIT License (with OpenAI/Anthropic Rider). See `LICENSE`.

## Round 1 Benchmark Script

```bash
./scripts/benchmark_round1.sh
```

## Round 2 Benchmark Script

```bash
./scripts/benchmark_round2.sh
```

## Benchmark Regression Gate

```bash
./scripts/benchmark_gate.sh
```

Use `FR_BENCH_THROUGHPUT_DROP_PCT` and `FR_BENCH_P99_REGRESSION_PCT` to tune
the failure thresholds. Each run writes raw benchmark reports, per-workload
comparisons, and an aggregate gate report under `artifacts/benchmark/<run-id>/`.
