# Pass 131 Rejected: Quanta GET Command Timer

- Bead: `frankenredis-q81y2`
- Agent: `TealOtter`
- Target: residual command-duration clock overhead in the borrowed GET fast path.
- Candidate: add `quanta 0.12.6` and use a global safe `quanta::Clock` raw timestamp bracket for `execute_plain_get_borrowed_into_with_default_read_gate`, pre-initialized at `Runtime::new` so calibration does not hit the first command.

## Baseline and Profile

- Current binary: reused `/tmp/rch_target_fr_3cc4w_p130_current/release-perf/frankenredis`; source-identical to pushed `0cce71be3` because pass130 committed evidence only.
- Candidate build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_q81y2_p131_candidate cargo build --profile release-perf -p fr-server -p fr-bench`
- Pass130 baseline GET/P16/C50/1M: `654.8 ms +/- 31.0 ms`
- Pass130 fresh profile rows supporting the deeper primitive: `[vdso] 0x983` `4.66%`, `[vdso] 0x937` `2.21%`, `Runtime::execute_plain_get_borrowed_into_with_default_read_gate` `1.28%`, `process_buffered_frames` `1.23%`, `Store::get_string_bytes` `0.96%`, and command histogram `record_with_kind` `0.47%`.

## Behavior Proof

- Byte-identical current/candidate TCP transcript: `8587a57563629a5f0674c348760cd25ad54b43ad6b4179b0f05c7cfda49c031a`
- Transcript length: `80` bytes for current and candidate.
- Transcript covered `SET`, default-threshold `GET`, `CONFIG SET latency-monitor-threshold 1`, `CONFIG GET latency-monitor-threshold`, threshold-enabled `GET`, `LATENCY LATEST`, and `QUIT`.
- Isomorphism note: response bytes, ordering, key/value semantics, selected DB, expiry ordering, floating-point behavior, RNG behavior, and command side effects are preserved. The candidate changed only the monotonic duration primitive used for the GET commandstats/slowlog/latency/threat elapsed value.

## Gates

- `cargo test -p fr-runtime plain_get_borrowed_into_cached_gate_matches_uncached -- --nocapture`: passed via `rch` remote `vmi1227854`, with known pre-existing test-only `unused_mut` warning.
- Candidate release build: passed via `rch` remote `vmi1227854`.

## Benchmark Result

Paired GET/P16/C50/1M:

- Current mean: `2.056 s +/- 0.236 s`
- Candidate mean: `2.079 s +/- 0.199 s`
- Hyperfine summary: current `1.01x +/- 0.15` faster than candidate.

Reversed GET/P16/C50/1M:

- Candidate mean: `1.972 s +/- 0.152 s`
- Current mean: `1.943 s +/- 0.216 s`
- Hyperfine summary: current `1.01x +/- 0.14` faster than candidate.

## Decision

Rejected. The deeper safe-clock candidate does not produce a measurable win and slightly loses in both orders. No source, dependency, or lockfile change is kept.

Next route: do not repeat GET clock-source substitutions until a lower-overhead primitive has a clearer proof path. Move to the shifted non-clock surfaces from the same profile: writer completion polling, unconditional empty pub/sub drain, output accounting, or store lookup/hash path.
