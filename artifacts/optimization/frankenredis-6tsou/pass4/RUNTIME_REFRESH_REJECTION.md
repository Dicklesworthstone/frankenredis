# frankenredis-6tsou Pass 4 - Runtime Refresh Rejection

Timestamp: 2026-06-08T04:24:00Z

## Lever

Candidate: make `Runtime::refresh_store_runtime_info_context()` conditional in
generic dispatch, refreshing only before cold observability/persistence commands
(`INFO`, `MEMORY`, `CLIENT`, `WAITAOF`).

Profile target: Pass 2 `getset-hit` profile showed
`Runtime::refresh_store_runtime_info_context` at 2.64% children / 1.45% self.

## Build

- Baseline binary: `/tmp/codex-fr-6tsou-getset-base-target/release-perf/frankenredis`
- Baseline sha256: `8bb4ad8a6aac2d16ca51ef2c8bca1b2cc24357365ab2f8f9d4c29fa363554732`
- Candidate binary: `/tmp/codex-fr-6tsou-refresh-candidate-target/release-perf/frankenredis`
- Candidate sha256: `ee699e78d2196134e10b2771a5a1467c254a312ef88e361cf46edbfa5588ee4d`
- Candidate build: `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6tsou-refresh-candidate-target cargo build --profile release-perf -p fr-server`
- RCH worker: `vmi1149989`

## Behavior Proof

Deterministic runtime-info transcript:

```text
baseline  sha256 = 72e957a0df5e726e8ebec122e2e11dbac095d2a696fdb73ae8c5a669a624aeb2
candidate sha256 = 72e957a0df5e726e8ebec122e2e11dbac095d2a696fdb73ae8c5a669a624aeb2
ISOMORPHISM (candidate==baseline): True
```

Focused checks:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6tsou-test-runtime cargo test -p fr-runtime info_clients_reads_blocked_clients_from_runtime_context -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6tsou-test-command cargo test -p fr-command info_reports_live_client_and_memory_context -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6tsou-test-command cargo test -p fr-command memory_stats_under_resp2_stays_flat_array -- --nocapture`

All passed. The final `fr-command` focused check ran locally through RCH fail-open
because no worker slot was admissible; it remained crate-scoped.

Isomorphism:

- Ordering preserved: command execution and reply emission order unchanged.
- Tie-breaking unchanged: no data-structure ordering or key iteration changes.
- Floating-point unchanged: candidate did not alter INFO/MEMORY formatting code.
- RNG unchanged: candidate did not touch LFU/RNG paths.
- Expiration/TTL unchanged: active/lazy expiry paths were untouched.
- AOF/replication unchanged: propagation and write-denial paths were untouched.

## Benchmark

Paired `getset-hit`, 300000 requests, 50 clients, pipeline 16, keyspace 10000,
datasize 3:

| Binary | Mean |
| --- | ---: |
| Baseline | 2.193s +/- 0.024 |
| Candidate | 2.198s +/- 0.034 |

Hyperfine summary: baseline ran 1.00x +/- 0.02 faster than candidate.

## Decision

Reject. The candidate proved behavior but did not produce a measurable win.

Score after benchmark: Impact 0 x Confidence 4 / Effort 3 = 0.0, below the 2.0
keep gate.

Production source was restored to the original unconditional refresh path. The
artifact harness remains for future observability-proof work.

## Next Primitive

Do not continue per-command borrowed write fast paths or runtime refresh
micro-levers for this bead. The next profile-backed family is a deeper
parser/dispatch primitive: collapse repeated command metadata/classification
work into the already-parsed borrowed command token, or attack zero-copy RESP
framing/inline small replies with a dedicated profile and proof plan.
