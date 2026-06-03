# Pass 30 Rejection Proof - Integer String Memory Estimation

## Target Selection

- Bead: `frankenredis-qap7w`.
- Initial hypothesis: recv/parser-side work after the pass29 response-segment rejection.
- Fresh current-HEAD baseline, rch-built at `08439b177`:
  - Direct SET pipeline=16: `293429.18 ops/sec`, p50 `2501 us`, p95 `3625 us`, p99 `4723 us`.
  - Hyperfine: `1.612s +/-0.038s`.
- Strace on the live server during 500k SET pipeline=16:
  - `sendto`: `31,250` calls / `55.70%`.
  - `recvfrom`: `62,550` calls / `40.85%`, including `31,250` WouldBlock errors.
  - `epoll_wait`: `1,090` calls / `2.17%`.
- Local `mio-1.2.0` source confirms epoll registration uses `EPOLLET`, so skipping drain-to-WouldBlock would repeat an unsound one-read shortcut.
- GDB sampling during the same workload found CPU samples in `is_int_encoded_string -> i64::to_string -> String push/copy` under `estimate_string_value_memory_usage_bytes` / `estimate_entry_memory_usage_bytes`.

## Candidate

One lever only: replace `is_int_encoded_string`'s `from_utf8 -> parse::<i64> -> n.to_string() == s` canonicality check with an allocation-free byte-level canonical signed i64 check.

This preserved the exact old accepted forms:

- Accepted: `0`, positive decimal without leading zeroes up to `i64::MAX`, negative decimal without `-0` / leading zeroes down to `i64::MIN`.
- Rejected: `+1`, leading zeroes, `-0`, overflows, whitespace, decimals, non-digits, non-UTF8 bytes.

## Validation Before Benchmark

- `rustfmt --edition 2024 crates/fr-store/src/lib.rs` and `rustfmt --edition 2024 --check crates/fr-store/src/lib.rs`: passed.
- `rch exec -- cargo test -p fr-store int_encoded_string_detection_preserves_canonical_i64_forms -- --nocapture`: passed.
- `rch exec -- cargo check -p fr-store --all-targets`: passed.
- Candidate release-perf build via `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`: passed.

## Behavior Proof

- Golden trace files: `int-baseline-golden.resp` and `int-candidate-golden.resp`.
- Golden command stream covered `SET`, `MEMORY USAGE`, and `GET` for canonical ints, non-canonical ints, signed boundaries, overflow, and non-UTF8 input.
- Golden SHA-256 matched byte-for-byte: `95bf66c334f215e45952c101db3ebeced83ee83074f1654aca19f1c6a7bb04e9`.
- Ordering/tie-breaking: command order and per-key replies are unchanged.
- Floating-point/RNG: not involved.

## Performance

- Fresh direct A/B:
  - Baseline: `299429.99 ops/sec`, p99 `4499 us`.
  - Candidate: `297971.21 ops/sec`, p99 `4323 us`.
- Paired hyperfine:
  - Baseline: `1.590s +/-0.028s`.
  - Candidate: `1.635s +/-0.032s`.
  - Baseline ran `1.03x +/-0.03x` faster.

## Verdict

Score is below the `2.0` keep gate because the profiler-relevant TCP workload regressed. The source hunk and test were removed; final `git diff -- crates/fr-store/src/lib.rs --exit-code` passed.

Next attack should be structurally deeper than this micro-lever: avoid periodic full memory-estimate scans on the hot path via cached per-entry memory deltas or sample-gated stats refresh, with a fresh profile and golden MEMORY USAGE proof before any source edit.
