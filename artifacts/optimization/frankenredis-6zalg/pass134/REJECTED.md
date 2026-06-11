# Pass 134 Rejected: Small Bulk Length Encoder

- Bead: `frankenredis-6zalg`
- Target: GET/P16/C50 response encoding after pure GET run batching was rejected.
- Candidate: specialize `fr_protocol::encode_bulk_string_slice` for bulk string lengths `0..=9` by writing the single ASCII length digit directly, avoiding `decimal_usize_len` and `push_usize` on the common 3-byte GET payload.

## Baseline And Profile

- Current release-perf build: `/tmp/rch_target_fr_6zalg_current/release-perf/frankenredis`; `rch` fell back locally because no admissible workers were available.
- Baseline GET/P16/C50/1M: `920.6 ms +/- 52.0 ms`.
- Fresh profile GET/P16/C50/3M: `1,598,915.23 ops/sec`, p50 `461us`, p95 `706us`, p99 `903us`, p999 `1378us`; 2953 samples, 0 lost.
- Relevant profile rows: `[vdso]` `5.66%` and `2.80%`, `Store::get_string_bytes` `1.34%`, `execute_plain_get_borrowed_into_with_default_read_gate` `0.90%`, `encode_bulk_string_slice` `0.84%`, `process_buffered_frames` `0.70%`, `foldhash::RandomState::hash_one` `0.65%`, `drain_writer_completions` `0.42%`, `drain_pending_pubsub_to_connection` `0.34%`.
- The profile also showed writer-thread syscall/send children as the larger remaining structural family.

## Behavior Proof

- Byte-identical current/candidate raw TCP transcript:
  - request sha256: `b1922dbd076a6507bb971558791bbf73cf24f44dbc94c5f00e2a0eefd5d08177`
  - response sha256: `3080c695b486981aabab05a2ed0caa3427f52e806b2197c083765186bae361ac`
- Transcript covered `SET`, `GET` hit with 3-byte payload, `GET` miss/null, empty bulk value, 10-byte bulk value, wrongtype `GET`, `SELECT`, `CLIENT REPLY OFF/ON`, `CLIENT TRACKING ON/OFF`, and `QUIT`.
- Isomorphism: reply bytes, command order, selected-DB behavior, client-reply suppression, tracking state, wrongtype errors, null bulk representation, tie-breaking, floating-point behavior, and RNG behavior are unchanged. The candidate only changed how an already-known bulk length was written into the output buffer.

## Gates

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_6zalg_candidate cargo build --profile release-perf -p fr-server -p fr-bench`: passed; `rch` local fallback.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_6zalg_candidate cargo test -p fr-protocol borrowed_bulk_slice_encoder_matches_frame_encoder -- --nocapture`: passed while candidate was applied.

## Benchmark Result

Paired GET/P16/C50/1M:

- Current mean: `750.6 ms +/- 63.9 ms`
- Candidate mean: `761.0 ms +/- 39.7 ms`
- Hyperfine summary: current `1.01x +/- 0.10` faster.

Reversed GET/P16/C50/1M:

- Candidate mean: `827.0 ms +/- 43.0 ms`
- Current mean: `781.8 ms +/- 35.6 ms`
- Hyperfine summary: current `1.06x +/- 0.07` faster.

## Decision

Rejected. The candidate is behavior-clean but slower in both benchmark orders and does not meet Score >= 2.0. No production source hunk is retained; the rejected patch is saved at `artifacts/optimization/frankenredis-6zalg/pass134/candidate/source-hunk-under-test.patch`.

Next route: do not repeat response integer/length micro-formatting. Attack a structurally larger primitive from the fresh profile: writer/syscall output batching, output-buffer cursor/ring layout, or a deeper store/key lookup layout change with a new profile and proof bundle.
