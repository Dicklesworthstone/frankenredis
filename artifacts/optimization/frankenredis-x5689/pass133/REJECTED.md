# Pass 133 Rejected: Pure GET Run Batching

- Bead: `frankenredis-x5689`
- Target: amortize per-command output-limit/pubsub/writer checks across contiguous canonical borrowed GET packets in `process_buffered_frames`.
- Candidate: detect a run of canonical `GET` packets under the default unlimited output-buffer hard limit, execute them through the existing cached default read gate, and drain/check output once at the run boundary. Non-GET, noncanonical, configured output-limit, tracking, pub/sub, monitor, replica/AOF, paused, transaction, non-db0, and other generic-observed states stayed on the existing per-command path.

## Baseline And Profile

- Current release-perf build: `/tmp/rch_target_fr_x5689_current/release-perf/frankenredis`; `rch` fell back locally because no admissible workers were available.
- Baseline GET/P16/C50/1M: `868.0 ms +/- 116.1 ms`.
- Fresh profile GET/P16/C50/3M: `1,573,167.33 ops/sec`, p50 `462us`, p95 `732us`, p99 `1078us`, p999 `1945us`; 3025 samples, 0 lost.
- Relevant profile rows: `process_buffered_frames` `0.67%`, `drain_pending_pubsub_to_connection` `0.60%`, `drain_writer_completions` `0.33%`, pubsub outbox `remove` `0.29%`, `get_string_bytes` `1.74%`, `encode_bulk_string_slice` `0.77%`, plus dominant clock/vdso rows.

## Graveyard / Artifact Route

- Mapped primitive: vectorized/batched execution over a contiguous data-plane run, from the graveyard batching/vectorized-execution family and the latency-decomposition contract.
- EV before implementation: Impact `2.0` x Confidence `3.0` / Effort `2.0` = `3.0`.
- Fallback trigger: any paired/reversed disagreement or current-faster result rejects the source hunk.
- Primary risk: delayed output/pubsub boundary or changed hard-output-limit behavior. Countermeasure: only batch under unlimited hard output limit, stop before non-GET, and golden-test `SELECT`, `CLIENT REPLY`, and tracking state transitions.

## Behavior Proof

- Byte-identical current/candidate raw TCP transcript:
  - request sha256: `080ffc877764372c588cd09117c8f65cc854b32e98e9bd534b2d12fbbedb9e54`
  - response sha256: `fef02389a599a6d4a395fbaa1f80b47c6cdaadced0e0d05c6d8f11dfc4cbd04f`
- Transcript covered `SET`, contiguous `GET` hit/miss, `SELECT` invalidation, `CLIENT REPLY OFF/ON` suppression, `CLIENT TRACKING ON/OFF`, final `GET`, and `QUIT`.
- Isomorphism: reply bytes, command order, selected-DB behavior, client-reply suppression, tracking gate behavior, keyspace hit/miss accounting, lazy-expiry propagation order, LRU/LFU/RNG behavior, floating-point behavior, commandstats/slowlog/latency/errorstats inputs, and output-limit hard-boundary semantics are unchanged by construction. The candidate used the same runtime GET executor.

## Gates

- `cargo fmt -p fr-server -- --check`: passed.
- `rch exec -- cargo check -p fr-server --all-targets`: passed.
- `rch exec -- cargo clippy -p fr-server --all-targets -- -D warnings`: passed.
- `rch exec -- cargo test -p fr-server process_buffered_frames_get_run_stops_before_generic_state_change -- --nocapture`: passed while candidate was applied.
- `rch exec -- cargo test -p fr-server canonical_get_run_respects_configured_output_limit_boundary -- --nocapture`: passed while candidate was applied.
- `rch exec -- cargo test -p fr-runtime pubsub -- --nocapture`: passed; known pre-existing test-only `unused_mut` warning remained.
- `rch exec -- cargo test -p fr-server subscribe_mode_gate_runs_arity_before_context_gate_nnbig -- --nocapture`: passed.

## Benchmark Result

Paired GET/P16/C50/1M:

- Current mean: `768.3 ms +/- 65.4 ms`
- Candidate mean: `784.4 ms +/- 52.8 ms`
- Hyperfine summary: current `1.02x +/- 0.11` faster.

Reversed GET/P16/C50/1M:

- Candidate mean: `856.0 ms +/- 56.7 ms`
- Current mean: `817.5 ms +/- 50.7 ms`
- Hyperfine summary: current `1.05x +/- 0.10` faster.

## Decision

Rejected. The candidate is behavior-clean but slower in both benchmark orders and does not meet Score >= 2.0. No production source hunk is retained; the rejected patch is saved at `artifacts/optimization/frankenredis-x5689/pass133/candidate/source-hunk-under-test.patch`.

Next route: do not repeat per-loop pure GET batching around pubsub/output checks. Re-profile current main and attack a different primitive, likely store read/hash layout, bulk-string encoding, or a lower-level timing/accounting replacement that removes a larger class of work without adding extra parser/control-flow overhead.
