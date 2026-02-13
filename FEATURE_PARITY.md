# FEATURE_PARITY

## Status Legend

- not_started
- in_progress
- parity_green
- parity_gap

## Parity Matrix

| Feature Family | Status | Notes |
|---|---|---|
| RESP protocol and command dispatch | in_progress | parser + bootstrap commands (`PING/ECHO/SET/GET/DEL/INCR/EXPIRE/PTTL`) implemented in strict mode with Redis-style arity/unknown-command strings |
| Core data types and keyspace | in_progress | string path implemented; non-string types pending |
| TTL and eviction behavior | in_progress | lazy expiry and `PTTL` semantics scaffolded (`-2/-1/remaining`) |
| RDB/AOF persistence | in_progress | AOF record frame contract scaffolded; full replay fidelity pending |
| Replication baseline | in_progress | state/offset progression scaffolded; protocol sync semantics pending |
| ACL/config mode split | not_started | policy model exists; ACL behavior parity not yet implemented |
| Differential conformance harness | in_progress | fixture runner online for `core_strings`, `core_errors`, `protocol_negative`, and `persist_replay` suites |
| Benchmark + optimization artifacts | in_progress | round1 + round2 baseline JSON, syscall profile, and expanded golden checksum artifacts added |

## Required Evidence Per Feature Family

1. Differential fixture report.
2. Edge-case/adversarial test results.
3. Benchmark delta (when performance-sensitive).
4. Documented compatibility exceptions (if any).

## Current Evidence Pointers

- `crates/fr-conformance/fixtures/core_strings.json`
- `crates/fr-conformance/fixtures/core_errors.json`
- `crates/fr-conformance/fixtures/protocol_negative.json`
- `crates/fr-conformance/fixtures/persist_replay.json`
- `baselines/round1_conformance_baseline.json`
- `baselines/round1_conformance_strace.txt`
- `baselines/round2_protocol_negative_baseline.json`
- `baselines/round2_protocol_negative_strace.txt`
- `golden_checksums.txt`
