# frankenredis-svgvb rejection report

## Target

- Bead: `frankenredis-svgvb`
- Lever tested: conservative borrowed `SETEX` / `PSETEX` write fast path.
- Workload: alternating `SETEX key 86400 value` and `PSETEX key 86400000 value`,
  300k or 1M requests, 50 clients, pipeline 16, keyspace 10k, value size 3.
- Baseline source: `cadb985ad`.

## Profile basis

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-svgvb-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Baseline SETEX/PSETEX P16/1M profile:

- `Store::run_active_expire_cycle`: 7.22% self
- `__memcmp_avx2_movbe`: 6.22% self
- BTree expiry iterator/range rows: 3.64% + 3.27% self
- `Runtime::execute_frame_internal`: 2.42% self
- `Runtime::refresh_store_runtime_info_context`: 2.38% self
- `__memmove_avx_unaligned_erms`: 2.31% self
- `parse_command_args_borrowed_into`: 1.32% self
- `fr_command::command_key_indexes`: 1.31% self
- `fr_command::command_table_index`: 1.09% self
- `process_buffered_frames`: 1.06% self

The profile justified one bounded borrowed-write attempt, but it also showed
that TTL-index/active-expire work dominates this command family.

## Behavior proof

Golden transcript covered:

- valid `SETEX` and `PSETEX`
- expiry-state proof through `PERSIST`
- lower/mixed-case command names
- invalid TTL fallback cases
- non-DB0 fallback
- `MULTI`/`EXEC` fallback

SHA-256:

```text
baseline  dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7  992 bytes
candidate dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7  992 bytes
```

Isomorphism:

- Ordering: preserved; command execution and reply emission stayed in the same
  per-client buffered order.
- Tie-breaking: unchanged; no data-structure ordering result changed.
- Floating point: N/A.
- RNG: unchanged.
- Expiry behavior: valid TTLs set the same relative expiry; invalid TTLs and
  disabled states fell back to generic dispatch.

Validation while candidate was applied:

```text
cargo fmt -p fr-runtime -p fr-server --check
python3 -m py_compile artifacts/optimization/frankenredis-svgvb/setex_bench.py artifacts/optimization/frankenredis-svgvb/run_setex_bench_once.py artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-svgvb-check-target cargo check -p fr-runtime -p fr-server --all-targets
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-svgvb-test-target cargo test -p fr-runtime plain_setex_psetex_borrowed -- --nocapture
```

Candidate release build was attempted through `rch`, but `rch` twice fell back
locally because workers were excluded or under pressure. The build remained
crate-scoped:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-svgvb-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

## Benchmarks

Standalone baseline rerun:

- baseline SETEX/PSETEX P16/300k: `1.373 s +/- 0.020 s`

Paired P16/300k:

- baseline: `1.597 s +/- 0.341 s`
- candidate: `1.475 s +/- 0.045 s`
- candidate: `1.08x +/- 0.23`, too noisy for keep.

Reversed P16/1M:

- candidate: `4.489 s +/- 0.063 s`
- baseline: `4.359 s +/- 0.210 s`
- baseline: `1.03x +/- 0.05` faster.

## Decision

Rejected. Score `0.0` because the confirmation run favored baseline and the
candidate did not meet the Score >= 2.0 keep gate. The production source hunk
was removed; only this evidence bundle and bead bookkeeping are retained.

## Next route

Do not repeat borrowed command stubs for TTL-heavy writes. The next
algorithmically different target is the TTL index itself: replace or sidecar the
`volatile_keys` BTree/range active-expire path with a bucketed timing-wheel or
expiry-heap primitive that preserves Redis-visible lazy expiry, active expiry
sampling, stale percentage stats, propagation order, and deterministic golden
transcripts.
