# frankenredis-ohsk5.5 rejection

## Target

- Bead: `frankenredis-ohsk5.5`
- Lever: lazy INFO runtime context refresh for the P16 hot path
- Fresh profile directory:
  `artifacts/optimization/orangemouse-pass94-current-20260609/cross-workload-profile/`

Cross-workload current-main P16/1M profile evidence:

- SET: `648953.65 ops/sec`; `Runtime::refresh_store_runtime_info_context`
  was `4.51%` self.
- GET: `711203.54 ops/sec`; `Runtime::refresh_store_runtime_info_context`
  was `5.52%` self.
- HSET: `615979.97 ops/sec`; `Runtime::refresh_store_runtime_info_context`
  was `4.97%` self.

## Lever Tested

The candidate removed the per-command `refresh_store_runtime_info_context()`
calls from generic dispatch and plain borrowed fast paths, then refreshed the
same INFO-facing store fields at INFO generation time.

The intent was to stop recomputing tracking counts, persistence flags, and
replication backlog memory on every hot command while preserving INFO output.

## Behavior Proof While Applied

- `cargo test -p fr-runtime info_ -- --nocapture` passed through `rch` local
  fallback: 33 runtime INFO tests and 9 admin INFO tests passed.
- `cargo build --profile release-perf -p fr-server -p fr-bench` passed for the
  candidate.
- `cargo fmt -p fr-runtime -- --check` was run and failed on broad pre-existing
  formatting drift in `crates/fr-runtime/src/lib.rs`, outside this source hunk.
  The file was not autoformatted because that would mix unrelated formatting
  churn into the perf lever.

## Benchmarks

Initial current-main baselines before the source edit:

- SET P16/300k: `493.6ms +/- 24.9ms`
- GET P16/300k: `484.7ms +/- 15.7ms`
- HSET P16/300k: `545.5ms +/- 6.4ms`

Initial candidate run:

- SET P16/300k: `484.6ms +/- 16.5ms`
- GET P16/300k: `472.0ms +/- 21.1ms`
- HSET P16/300k: `777.7ms +/- 126.1ms`

During the pass an unrelated shared-tree `fr-command` edit appeared after the
initial baseline build. To avoid scoring contaminated numbers, the source hunk
was removed, a same-tree no-hunk baseline was rebuilt, and HSET was paired
against the already-built candidate:

- Same-tree no-hunk HSET P16/300k: `557.2ms +/- 48.0ms`
- Same-tree candidate HSET P16/300k: `521.4ms +/- 26.7ms`
- Hyperfine summary: candidate `1.07x +/- 0.11` faster

## Decision

Reject under the Score>=2.0 rule. The same-tree confirmation showed only a
small noisy HSET win, and SET/GET also showed only small directional movement.

- Impact: `1.0`
- Confidence: `2.0`
- Effort: `1.5`
- Score: `1.33`

The production source hunk was removed. No runtime code from this candidate is
retained.

## Next Route

Do not repeat lazy INFO-refresh or command-observability micro-levers. Pass 95
should attack a deeper primitive with broader profile support:

- reply/output batching if `try_flush` / `__send` child cost remains dominant
  under a userspace-focused profile; or
- safe-Rust keyspace/fingerprint layout if key hashing and `internal_entry`
  dominate across SET/HSET after isolating write-side cost.
