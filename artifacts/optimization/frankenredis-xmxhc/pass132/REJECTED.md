# Pass 132 Rejected: Empty Pub/Sub Drain Guard

- Bead: `frankenredis-xmxhc`
- Agent: `TealOtter`
- Target: pure GET pipeline overhead from unconditional empty Pub/Sub drain.
- Candidate: add `Store::has_pending_pubsub`, `Runtime::has_pending_pubsub_for_current_client`, and an early return in `drain_pending_pubsub_to_connection` when both the legacy store queue and current-client outbox are empty.

## Baseline

- Current binary: `/tmp/rch_target_fr_3cc4w_p130_current/release-perf/frankenredis`; source-identical for server/runtime/store hot path to pushed `9730775a5`.
- Fresh current baseline GET/P16/C50/1M: `847.9 ms +/- 31.0 ms`
- Prior profile rows supporting the trial: `drain_writer_completions` `0.61%`, pub/sub outbox remove `0.50%`, `drain_pending_pubsub_to_connection` `0.46%`, and nearby output/accounting rows after the clock-source rejections.

## Behavior Proof

- Byte-identical current/candidate TCP transcript: `8587a57563629a5f0674c348760cd25ad54b43ad6b4179b0f05c7cfda49c031a`
- Transcript length: `80` bytes for current and candidate.
- Transcript covered `SET`, default-threshold `GET`, `CONFIG SET latency-monitor-threshold 1`, `CONFIG GET latency-monitor-threshold`, threshold-enabled `GET`, `LATENCY LATEST`, and `QUIT`.
- Existing Pub/Sub/RESP3/tracking gates passed: `cargo test -p fr-runtime pubsub -- --nocapture` and `cargo test -p fr-server subscribe_mode_gate_runs_arity_before_context_gate_nnbig -- --nocapture`.
- Isomorphism note: response bytes, ordering, subscription mode behavior, RESP2/RESP3 push framing, client tracking invalidation routing, floating-point behavior, RNG behavior, and commandstats/slowlog/latency inputs are preserved. The candidate only avoided draining when no message source had pending messages.

## Benchmark Result

Paired GET/P16/C50/1M:

- Current mean: `728.7 ms +/- 46.1 ms`
- Candidate mean: `710.4 ms +/- 65.0 ms`
- Hyperfine summary: candidate `1.03x +/- 0.11` faster than current.

Reversed GET/P16/C50/1M:

- Candidate mean: `786.7 ms +/- 60.3 ms`
- Current mean: `763.3 ms +/- 38.9 ms`
- Hyperfine summary: current `1.03x +/- 0.09` faster than candidate.

## Decision

Rejected. The paired and reversed orders disagree by the same magnitude, so the candidate does not meet Score >= 2.0. No source change is kept.

Next route: avoid more branch guards around empty drains. The next attack should change the structure: batch writer completion polling around wake-token activity only, or move pure borrowed GET processing into a tighter loop that amortizes output-limit/pubsub/writer checks across a run while preserving immediate delivery boundaries for commands that can generate pushes.
