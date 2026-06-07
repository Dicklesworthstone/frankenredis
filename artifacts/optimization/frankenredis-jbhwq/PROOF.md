# frankenredis-jbhwq HMGET Borrowed Fast Path Proof

## Scope

One lever: add a conservative borrowed runtime fast path for plain
`HMGET key field [field ...]` requests. The generic command path remains the
fallback for parse errors, non-default runtime states, disabled policy states,
or arity/gate limit failures.

## Build Inputs

- Baseline source: `25ec4b37e830`
- Baseline binary: `/tmp/codex-fr-jbhwq-baseline-target/release-perf/frankenredis`
- Baseline binary sha256:
  `79a08688e298f6e28bdb55484c7a8ccc01bb8c26b952c96344734fba7c8e65dc`
- Candidate binary: `/tmp/codex-fr-jbhwq-nobleriver-candidate-target/release-perf/frankenredis`
- Candidate binary sha256:
  `d2aadf0f63791b6dbf383f4bdaa5c6a69ee739b7aaa41c6c4f62ba83839bd7e1`

## Golden Output

Golden input: `hmget-golden-input.resp`

Output sha256 is byte-identical for baseline, candidate, and Redis 7.2.4:

```text
3955a8a6d854e8c8a2fae717ab77183aa70ff733f002b89d3d77c2f1f2f73a1c  baseline-hmget-golden-output.resp  199 bytes
3955a8a6d854e8c8a2fae717ab77183aa70ff733f002b89d3d77c2f1f2f73a1c  candidate-hmget-golden-output.resp  199 bytes
3955a8a6d854e8c8a2fae717ab77183aa70ff733f002b89d3d77c2f1f2f73a1c  redis724-hmget-golden-output.resp  199 bytes
```

## Isomorphism Notes

- Ordering: preserved by passing borrowed field slices to `Store::hmget` in the
  original request order and constructing the reply array from that order.
- Tie-breaking: not applicable; HMGET is a direct positional read.
- Floating point: not applicable.
- RNG: not applicable.
- Error semantics: WRONGTYPE is returned through the same `CommandError::Store`
  mapping as generic dispatch.
- Disable states: fast path uses the shared default borrowed-read gate and
  returns `None` for fallback under non-default states such as MULTI.
- Metrics: session command name, argv length sum, read counters, errorstats,
  slowlog, latency tracking, and command histograms mirror the existing borrowed
  read fast-path pattern.

## Benchmarks

Primary reversed-order benchmark:

Command:

```text
FIELD_COUNT=5 REQUESTS=1000000 hyperfine --warmup 2 --runs 8 --export-json artifacts/optimization/frankenredis-jbhwq/hmget-5field-reversed-1m-hyperfine.json \
  'bash artifacts/optimization/frankenredis-jbhwq/run_hmget_bench.sh candidate-rev /tmp/codex-fr-jbhwq-nobleriver-candidate-target/release-perf/frankenredis 21669' \
  'bash artifacts/optimization/frankenredis-jbhwq/run_hmget_bench.sh baseline-rev /tmp/codex-fr-jbhwq-baseline-target/release-perf/frankenredis 21668'
```

Result:

- Candidate: `1.77381138945s +/- 0.03099483874s`
- Baseline: `2.96075529145s +/- 0.07757042054s`
- Delta: candidate `1.67 +/- 0.05x` faster
- Redis-benchmark last run: `584112.12 rps` candidate vs `346140.53 rps`
  baseline
- Hyperfine JSON sha256:
  `8f52adb47b1d6b79f052b427c0d64e2a702c33c3d514cba6e91de73568e9438d`

Earlier baseline-first check:

- Candidate: `2.092932412345s +/- 0.18836737775s`
- Baseline: `3.795269902845s +/- 0.78118586170s`
- Delta: candidate `1.81 +/- 0.41x` faster

Score: Impact 4 x Confidence 4 / Effort 3 = 5.33. Keep threshold is 2.0.

## Validation

Passed:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-jbhwq-check-target cargo check -p fr-runtime -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-jbhwq-test-target cargo test -p fr-runtime plain_hmget_borrowed_fast_path -- --nocapture`
- `cargo fmt -p fr-runtime -p fr-server --check`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-jbhwq-clippy-target cargo clippy -p fr-runtime -p fr-server --all-targets -- -D warnings`

Additional validation notes:

- `cargo test -p fr-server -- --nocapture` passed the touched parser/runtime
  unit paths, then failed `tcp_aof_restart_preserves_all_data`.
- The same targeted AOF test fails on the clean baseline worktree at
  `25ec4b37e830`, so it is not caused by the HMGET fast path.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-jbhwq-conformance-target cargo test -p fr-conformance -- --nocapture`
  fell back to local execution, showed existing protocol fixture failures
  (`bulk_length_overflow`, `multibulk_length_overflow`: expected limit error,
  actual unexpected EOF), then hung in the long conformance run and was
  terminated by PID after several silent polls.
- `ubs crates/fr-runtime/src/lib.rs crates/fr-server/src/main.rs` returned
  file-wide pre-existing findings in these large modules; clippy, fmt, and
  build/test compile gates were clean for the touched crates.
