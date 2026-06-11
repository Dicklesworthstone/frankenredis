# Pass 135 Rejected: Writer Handoff Spare Output Buffer

- Bead: `frankenredis-ja34o`
- Target: GET/P16/C50 writer/syscall/output path after small bulk-length encoding was rejected.
- Candidate: after a successful writer-thread handoff, leave the connection with an empty write buffer preallocated up to a bounded spare capacity derived from the in-flight batch. This tests an output-slab reuse primitive without changing writer ownership, queue order, or the one-in-flight rule.

## Baseline And Profile

- Current release-perf build: `/tmp/rch_target_fr_ja34o_current/release-perf/frankenredis`; `rch` fell back locally because no admissible workers were available.
- Baseline GET/P16/C50/1M: `794.4 ms +/- 72.4 ms`.
- Profile basis: pass134 kept no production source, so the fresh pass134 profile remains source-identical for server/runtime/protocol hot paths. GET/P16/C50/3M: `1,598,915.23 ops/sec`, p50 `461us`, p95 `706us`, p99 `903us`, p999 `1378us`; 2953 samples, 0 lost.
- Relevant rows: writer-thread `send`/syscall family dominated the children report; self rows included `drain_writer_completions` `0.42%`, `process_buffered_frames` `0.70%`, `Store::get_string_bytes` `1.34%`, and clock/vDSO rows.

## Behavior Proof

- Byte-identical current/candidate raw TCP transcript:
  - request sha256: `b1922dbd076a6507bb971558791bbf73cf24f44dbc94c5f00e2a0eefd5d08177`
  - response sha256: `3080c695b486981aabab05a2ed0caa3427f52e806b2197c083765186bae361ac`
- Transcript covered `SET`, `GET` hit/miss, empty and 10-byte bulk values, wrongtype `GET`, `SELECT`, `CLIENT REPLY OFF/ON`, `CLIENT TRACKING ON/OFF`, and `QUIT`.
- Isomorphism: reply bytes, command order, selected-DB behavior, client-reply suppression, tracking state, wrongtype errors, output hard-limit accounting by pending byte length, tie-breaking, floating-point behavior, and RNG behavior are unchanged. The candidate retained capacity only; it did not count capacity as pending output.

## Gates

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ja34o_candidate cargo test -p fr-server writer_handoff_leaves_bounded_spare_write_buffer -- --nocapture`: passed while candidate was applied.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ja34o_candidate cargo build --profile release-perf -p fr-server -p fr-bench`: passed; `rch` local fallback.

## Benchmark Result

Paired GET/P16/C50/1M:

- Current mean: `763.7 ms +/- 43.3 ms`
- Candidate mean: `747.3 ms +/- 69.3 ms`
- Hyperfine summary: candidate `1.02x +/- 0.11` faster.

Reversed GET/P16/C50/1M:

- Candidate mean: `647.3 ms +/- 38.9 ms`
- Current mean: `646.8 ms +/- 36.5 ms`
- Hyperfine summary: current `1.00x +/- 0.08` faster.

## Decision

Rejected. The candidate is behavior-clean but the paired signal disappears in reversed order and does not meet Score >= 2.0. No production source hunk is retained; the rejected patch is saved at `artifacts/optimization/frankenredis-ja34o/pass135/candidate/source-hunk-under-test.patch`.

Next route: do not repeat spare-capacity or plain Vec reuse variants. Re-profile with syscall counts and attack a structurally different output primitive: true writev/segmented output buffering, worker-side multi-buffer batching, or a deeper event-loop/write-readiness scheduling primitive with ordering proof.
