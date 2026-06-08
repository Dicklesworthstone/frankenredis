# frankenredis-3jcbz rejection report

## Target

- Bead: `frankenredis-3jcbz`
- Lever: gate `Runtime::refresh_store_runtime_info_context` to top-level `INFO`/`MEMORY` observer commands instead of running it on every `execute_dispatch`.
- Profile basis: GETSET P16/300k profile showed `refresh_store_runtime_info_context` at 0.89% self time.

## Baseline

- Harness: `artifacts/optimization/frankenredis-6tsou/run_resp_mode_once.py`
- Workload: `getset-hit`, 300000 requests, 50 clients, pipeline 16, keyspace 10000, datasize 3.
- Baseline binary: `/tmp/codex-fr-gogmd-baseline-target/release-perf/frankenredis`
- Fresh baseline: 2.13999859614s +/- 0.01267129681s.

## Behavior proof

- `cargo check -p fr-runtime --all-targets` via rch: passed.
- Raw RESP golden baseline SHA: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`.
- Raw RESP golden candidate SHA: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`.
- Normalized observer golden baseline SHA: `56abe321f8890ba1ad588221669cc15c1037714116406f6f721db70758f14190`.
- Normalized observer golden candidate SHA: `56abe321f8890ba1ad588221669cc15c1037714116406f6f721db70758f14190`.
- `cargo test -p fr-runtime --all-targets info -- --nocapture` via rch: 42 matching tests passed, 0 failed.

Isomorphism notes:

- Command ordering and commandstat ordering are unchanged; dispatch still increments `stat_total_commands_processed` before observer refresh.
- Tie-breaking/RNG/floating-point behavior is untouched; the lever only moved a deterministic runtime-info publication call.
- INFO/MEMORY-visible runtime fields were checked with normalized output hashes for tracking totals, maxmemory publication, and replication backlog publication.

## Paired benchmark

- Baseline paired mean: 2.14780622848s +/- 0.01304184038s.
- Candidate paired mean: 2.13414324888s +/- 0.02784440581s.
- Hyperfine summary: candidate ran 1.01 +/- 0.01 times faster.

## Decision

Rejected. The measured effect is inside noise and does not clear Score >= 2.0:

- Impact: low, approximately 0.64% mean speedup on this workload.
- Confidence: low to medium, because the candidate standard deviation is larger than the mean delta and ranges overlap.
- Effort: low, but not enough to offset the weak impact/confidence.
- Score: 0.5.

Next direction: stop pursuing per-command runtime-info micro-tuning and attack a deeper parser/event-loop primitive with a larger target ratio.
