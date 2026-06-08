# Pass 83: future-expiry overwrite guard rejected

Bead: `frankenredis-6kecb`

## Profile basis

Fresh pass83 baseline was built via RCH on `vmi1227854`:

```bash
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass83-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

One-sided SETEX/PSETEX alternate P16/1M baseline:

- Baseline: `4.4228570529s +/- 0.04199482589232197s`

Server-only `perf record -e cycles:u -F 499 -g` on the baseline showed the top
remaining rows had shifted toward expiry/runtime bookkeeping:

- `Store::drop_if_expired`: `8.46%`
- `__memcmp_avx2_movbe`: `8.29%`
- `Runtime::refresh_store_runtime_info_context`: `6.18%`
- `clock_gettime` via `execute_frame_internal` / active expire: `5.11%`
- `_mi_page_malloc_zero`: `4.82%`
- `Runtime::execute_frame_internal`: `2.97%`
- `process_buffered_frames`: `2.76%`
- `dispatch_with_client_context`: `2.10%`
- `rewrite_relative_expire_for_propagation`: `1.77%`
- `parse_command_args_borrowed_into`: `1.52%`

## Lever tested

Candidate added an overwrite-specific store helper for unconditional writes:
`Store::set` and `Store::set_with_abs_expiry` skipped their per-key
`drop_if_expired` probe when `expiry_deadline_counts` proved the global earliest
key expiry deadline was still in the future. If any key was due, the candidate
fell through to the existing `drop_if_expired` path, preserving lazy-expiry
stats, keyspace events, and DEL-before-SET propagation ordering.

This targeted the long-TTL SETEX/PSETEX workload, where the benchmark TTLs are
`86400s` and `86400000ms`, so no key can expire during the measured run.

The source hunk and candidate-only tests were removed after the benchmark
failed the keep gate.

## Validation while candidate was applied

- RCH `cargo check -p fr-store --all-targets` passed.
- RCH `cargo test -p fr-store unconditional_set_ -- --nocapture` passed:
  two candidate tests covered future-deadline overwrite side effects and
  due-deadline lazy-expire-before-overwrite propagation.
- RCH release-perf build for `fr-server` / `fr-bench` passed.
- `cargo fmt -p fr-store -- --check` still reports pre-existing unrelated
  rustfmt drift in sentinel code outside this candidate.

Binary hashes are recorded in `binaries-baseline.sha256` and
`binaries-candidate.sha256`.

## Golden output

Comparator:

```bash
python3 artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py 27243 27244 artifacts/optimization/frankenredis-6kecb/pass83-batched-dispatch-packet/golden-compare.json
```

Result:

- Baseline bytes: `992`
- Candidate bytes: `992`
- Baseline SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Candidate SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Equal: `true`

Isomorphism notes: the candidate did not change reply bytes, command ordering,
TTL parsing, deadline math, lazy-expiry ordering when any deadline was due,
keyspace notification ordering, AOF/replica propagation ordering, tie-breaking,
floating-point behavior, or RNG behavior.

## Benchmark

Paired hyperfine, baseline first:

- Baseline: `4.475179968068571s +/- 0.03218972006863589s`
- Candidate: `5.042082837925714s +/- 0.2992463342825748s`
- Summary: baseline `1.13x +/- 0.07x` faster than candidate.

Reversed hyperfine, candidate first:

- Candidate: `4.4361751511400005s +/- 0.04996650960078449s`
- Baseline: `4.4490487323400005s +/- 0.08204384556178136s`
- Summary: candidate `1.00x +/- 0.02x` faster.

## Decision

Rejected under the Score>=2.0 keep gate. The best confirmed result is neutral
and the baseline-first run showed a noisy regression, so the store
future-expiry guard is not retained.

Next route: stop probing unconditional-write expiry guards for `6kecb`.
Attack a wider propagation/dispatch packet: avoid repeated relative-expiry
command classification, command metadata lookup, and runtime-info refresh
work across the SETEX/PSETEX pipeline window while preserving the exact
SETEX/PSETEX golden SHA and DEL-before-SET lazy-expiry semantics.
