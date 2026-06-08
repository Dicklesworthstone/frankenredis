# frankenredis-jdiiv pass68 rejection proof

## Target

- Bead: `frankenredis-jdiiv`
- Parent: `c1b8024b4` plus existing remote code state; no production source from this pass kept.
- Profile-backed family: output/syscall batching after GET metadata aggregate rejection.
- Lever tested: in `ClientConnection::try_flush`, use `Vec::clear()` when `total_written == write_buf.len()` and keep the existing `drain(..total_written)` path for partial writes.

## Baseline/Profile

Build:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-jdiiv-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench`

GET P16 / 300k / 50 clients:

- Baseline: `520.0 ms +/- 8.2 ms`

GET P16 / 1M profile:

- Throughput: `730554.60 ops/sec`
- p99: `1576 us`
- Samples: 797

Profile highlights:

- `ClientConnection::try_flush`: 34.07% children, 0.16% self.
- `__send`: 33.60% children.
- `<fr_store::Value>::string_owned`: 9.55% self.
- `__memmove_avx_unaligned_erms`: 7.39% self, with a smaller branch under `try_flush`.
- `refresh_store_runtime_info_context`: 5.73% self / 14.56% children.

## Behavior Proof

Golden harness:

- `artifacts/optimization/frankenredis-5srqd-pass67/run_tracking_golden.py`

Result:

- Baseline SHA-256: `d4a9346460558d9cf4137cee8d76445677fb9de87b74475f5008600e46c8f17e`
- Candidate SHA-256: `d4a9346460558d9cf4137cee8d76445677fb9de87b74475f5008600e46c8f17e`
- Parity: true

Isomorphism notes:

- Command/reply ordering unchanged: candidate only changed the empty-after-full-write buffer cleanup.
- Partial-write behavior unchanged: partial writes still use `drain(..total_written)`.
- Floating-point unchanged: no FP operations involved.
- RNG unchanged: no RNG operations involved.

Validation:

- Candidate release build passed through RCH for `fr-server` + `fr-bench`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-jdiiv-test-target cargo test -p fr-server try_flush -- --nocapture` compiled and passed, but matched zero tests; no focused shipped test kept because the source hunk was rejected.
- `cargo fmt -p fr-server --check` is currently blocked by pre-existing formatting drift in the remote `fr-server` subscribe-gate test/import hunk, not by this rejected `try_flush` hunk.

## Benchmarks

GET P16 / 300k / 50 clients:

- Baseline: `520.0 ms +/- 8.2 ms`
- Candidate: `512.1 ms +/- 14.9 ms`

Paired GET P16 / 1M:

- Baseline: `1.567 s +/- 0.081 s`
- Candidate: `1.441 s +/- 0.023 s`
- Candidate: `1.09x +/- 0.06`

Reversed GET P16 / 1M:

- Candidate: `1.471 s +/- 0.061 s`
- Baseline: `1.495 s +/- 0.019 s`
- Candidate: `1.02x +/- 0.04`

## Decision

Rejected. Score is below the keep threshold:

- Impact: 1
- Confidence: 1
- Effort: 1
- Score: 1.0

No production source hunk is kept.

## Next Primitive

Do not keep iterating the same `try_flush` drain/partial-write loop. The current profile is dominated by kernel `__send`, so the next productive route is the already-ready command-family write fast path bead:

- `frankenredis-6tsou`: continue borrowed write fast paths after the APPEND keep.
- Target primitive: remove owned argv/materialization and generic dispatch for more write commands (`SETNX`, `GETSET`, `GETDEL`, `SETEX`/`PSETEX`) with byte-identical proof.
