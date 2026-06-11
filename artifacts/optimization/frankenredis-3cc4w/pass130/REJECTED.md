# Pass 130 Rejected: GET Latency Rounding Guard

- Bead: `frankenredis-3cc4w`
- Agent: `TealOtter`
- Target: residual command-duration clock/metrics cost in GET/P16/C50 after the cached GET read gate keep.
- Candidate: move the GET borrowed metrics `elapsed_us.div_ceil(1000)` calculation behind the `latency-monitor-threshold != 0` guard so the default disabled latency-monitor path skips dead rounding work.

## Baseline and Profile

- Current build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_3cc4w_p130_current cargo build --profile release-perf -p fr-server -p fr-bench`
- Baseline GET/P16/C50/1M: `654.8 ms +/- 31.0 ms`
- Fresh profile workload: GET/P16/C50/3M
- Profile throughput: `1,787,045.20 ops/sec`
- Profile latency: p50 `412us`, p95 `627us`, p99 `842us`, p999 `1327us`, max `16927us`
- Profile rows: `[vdso] 0x983` `4.66%`, `[vdso] 0x937` `2.21%`, `Runtime::execute_plain_get_borrowed_into_with_default_read_gate` `1.28%`, `process_buffered_frames` `1.23%`, `Store::get_string_bytes` `0.96%`, `encode_bulk_string_slice` `0.74%`, `drain_writer_completions` `0.61%`, foldhash `hash_one<&[u8]>` `0.54%`, pub/sub outbox remove `0.50%`, command histogram `record_with_kind` `0.47%`, `drain_pending_pubsub_to_connection` `0.46%`, `Store::drop_if_expired` `0.41%`, and `refresh_client_memory_aggregates` `0.40%`.

## Behavior Proof

- Byte-identical current/candidate TCP transcript: `8587a57563629a5f0674c348760cd25ad54b43ad6b4179b0f05c7cfda49c031a`
- Transcript length: `80` bytes for current and candidate.
- Transcript covered `SET`, default-threshold `GET`, `CONFIG SET latency-monitor-threshold 1`, `CONFIG GET latency-monitor-threshold`, threshold-enabled `GET`, `LATENCY LATEST`, and `QUIT`.
- Isomorphism note: response ordering, key/value semantics, selected DB, expiry ordering, floating-point behavior, RNG behavior, commandstats/slowlog/latency/threat duration inputs, and response bytes are preserved. The candidate only changed whether default-disabled latency monitoring computes an unused rounded millisecond duration.

## Gates

- `cargo test -p fr-runtime plain_get_borrowed_into_cached_gate_matches_uncached -- --nocapture`: passed; `rch` fell back locally, with known pre-existing test-only `unused_mut` warning.
- Candidate release build: passed; `rch` fell back locally.

## Benchmark Result

Paired GET/P16/C50/1M:

- Current mean: `658.1 ms +/- 46.0 ms`
- Candidate mean: `662.2 ms +/- 18.4 ms`
- Hyperfine summary: current `1.01x +/- 0.08` faster than candidate.

Reversed GET/P16/C50/1M:

- Candidate mean: `641.3 ms +/- 34.0 ms`
- Current mean: `677.3 ms +/- 31.3 ms`
- Hyperfine summary: candidate `1.06x +/- 0.07` faster than current.

## Decision

Rejected. The two orders disagree and the confidence interval overlaps noise, so this does not meet Score >= 2.0. No source change is kept.

Next route: do not continue micro-tuning GET metric arithmetic. The remaining clock cost is the command-duration primitive itself: the fast path still needs per-command elapsed microseconds for `SLOWLOG`, `LATENCY`, `INFO commandstats`, and threat logging. Attack a deeper safe clock/timing primitive, or route to the next profiled non-clock surfaces (`drain_writer_completions`, pub/sub drain, output accounting, or store lookup) with a fresh baseline.
