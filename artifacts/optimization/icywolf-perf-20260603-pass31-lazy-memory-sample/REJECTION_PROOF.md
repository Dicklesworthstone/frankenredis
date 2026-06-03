# Pass 31 Rejection Proof - Lazy Logical Memory Estimate

## Bead

- `frankenredis-orqa4` - `[perf] Lazy logical memory estimate during RSS sampling`
- Agent: `IcyWolf`
- Verdict: rejected after measurement; no source changes retained.

## Profile-Backed Target

Pass30 profiling sampled `Store::record_ops_sec_sample` through
`estimate_entry_memory_usage_bytes` / `estimate_string_value_memory_usage_bytes`.
The measured code eagerly computed `estimate_memory_usage_bytes()` before calling
`read_rss_bytes().unwrap_or(used_memory)`, so the logical estimate was paid even
when procfs RSS succeeded and the logical fallback was discarded.

## One Lever Tested

Candidate hunk only changed `record_ops_sec_sample` so the logical estimate was
computed lazily as the `read_rss_bytes()` fallback:

```rust
let used_memory_rss = read_rss_bytes().unwrap_or_else(|| self.estimate_memory_usage_bytes());
```

The hunk was removed after the benchmark gate failed. `crates/fr-store/src/lib.rs`
has no retained diff for this pass.

## Baseline and Validation

Baseline binary:

- `target-icywolf-pass30-baseline-rch/release-perf/frankenredis`
- `target-icywolf-pass30-baseline-rch/release-perf/fr-bench`

Candidate binary:

- `target-icywolf-pass31-lazy-candidate-rch/release-perf/frankenredis`
- `target-icywolf-pass31-lazy-candidate-rch/release-perf/fr-bench`

Candidate pre-benchmark validation:

- `rustfmt --edition 2024 --check crates/fr-store/src/lib.rs`
- `rch exec -- cargo test -p fr-store periodic_sampling_updates_rss_and_peak_memory_stats -- --nocapture`
- `rch exec -- cargo check -p fr-store --all-targets`
- `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`

## Performance Evidence

SET pipeline=16, 50 clients, 500k requests:

- Baseline direct: `281823.65 ops/sec`, p95 `3835 us`, p99 `4511 us`.
- Candidate direct: `264329.94 ops/sec`, p95 `4061 us`, p99 `4967 us`.

Initial paired hyperfine, 500k requests:

- Baseline: `1.636s +/- 0.064s`.
- Candidate: `1.595s +/- 0.046s`.
- Candidate appeared `1.03x +/- 0.05x` faster, conflicting with direct throughput.

Long paired hyperfine tie-breaker, 2M requests:

- Baseline: `6.884s +/- 0.263s`.
- Candidate: `7.034s +/- 0.095s`.
- Baseline ran `1.02x +/- 0.04x` faster.

The profiler-relevant workload did not clear the keep gate.

## Behavior Proof

Raw deterministic command trace:

- Commands: `SET`, `GET`, `MEMORY USAGE`, `DEL`, missing `GET`.
- Baseline and candidate raw RESP bytes matched exactly.
- sha256: `31eb164a64364842b2fc660a3dc7377f54a81eb313aa5219a04f63ca6366c977`.

INFO/stat trace:

- Commands: `SET`, `MEMORY USAGE`, `INFO memory`, `INFO stats`, `DEL`.
- Raw trace differences were limited to process-local RSS and event-loop timing counters.
- Normalized baseline and candidate traces matched exactly after masking:
  `used_memory_rss`, `used_memory_rss_human`, `used_memory_peak`,
  `used_memory_peak_human`, `allocator_resident`, `mem_fragmentation_ratio`,
  `eventloop_cycles`, `eventloop_duration_sum`, `eventloop_duration_cmd_sum`,
  and `instantaneous_eventloop_cycles_per_sec`.
- normalized sha256: `ddc8952d287835a0df686a80a77c17d965b1011139b68d9f02d8d79e9308a7cc`.

Isomorphism:

- Command ordering and response ordering are unchanged.
- Tie-breaking is not involved.
- The RSS-success path observes the same RSS sample value.
- The RSS-failure path computes the same logical fallback value when needed.
- Logical `MEMORY USAGE` and command/stat counters are unchanged.
- Floating point and RNG are not involved.

## Score

Score below `2.0`: direct throughput and the long paired hyperfine both rejected
the candidate.

## Next Primitive

Do not keep iterating sample-gated stats refresh. The next attack is cached
per-entry memory deltas / dirty memory accounting from the slab/cache-line
accounting family, so `record_ops_sec_sample` no longer needs a full logical
store walk when exact logical memory is needed.
