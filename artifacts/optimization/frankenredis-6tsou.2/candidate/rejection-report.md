# frankenredis-6tsou.2 rejection report

## Target

- Bead: `frankenredis-6tsou.2`
- Lever: parser-adjacent borrowed command-token classifier for the strict multibulk fast path in `fr-server`
- Workload: `getset-hit`, 300000 requests, 50 clients, pipeline 16, keyspace 10000, datasize 3
- Baseline binary: `/tmp/codex-fr-6tsou1-head-baseline-target/release-perf/frankenredis`
- Candidate binary: `/tmp/codex-fr-6tsou2-candidate-target/release-perf/frankenredis`

## Profile basis

Baseline perf was collected on `getset-hit` under `perf record -F 499 -g`.
Relevant sampled rows:

- `frankenredis::process_buffered_frames`: 0.41% self
- `fr_command::command_key_indexes`: 0.41% self
- `fr_command::command_table_index`: 0.45% self
- `fr_command::acl_command_selectors_for_argv`: 0.46% self
- `<fr_runtime::Runtime>::execute_frame_internal`: 1.01% self
- `<fr_runtime::Runtime>::refresh_store_runtime_info_context`: 1.13% self

The candidate tried to avoid the repeated borrowed fast-path detector chain for non-fast-path commands such as `GETSET` by classifying the borrowed command token once.

## Behavior proof

- Raw RESP golden comparison: equal
- Baseline SHA-256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- Candidate SHA-256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- Focused tests:
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6tsou2-process-tests cargo test -p fr-server process_buffered_frames -- --nocapture`: 2 passed
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6tsou2-reply-tests cargo test -p fr-server client_reply -- --nocapture`: 2 passed

Isomorphism: the candidate only changed which borrowed fast handler was attempted before falling back to the existing generic argv copy and dispatch path. Ordering, tie-breaking, floating point, and RNG behavior are unaffected by this path.

## Benchmark result

Baseline one-sided hyperfine:

- `getset-hit` baseline: `3.06849385626s +/- 0.05700454929s`

Paired same-host hyperfine:

- Baseline: `2.15283560896s +/- 0.03896322977s`
- Candidate: `2.15309791606s +/- 0.01650475765s`
- Hyperfine summary: baseline ran `1.00 +/- 0.02x` faster than candidate

## Decision

Rejected. Score: `0.0` because the candidate was neutral on the profiled workload and does not meet the Score >= 2.0 keep threshold.

The production hunk was removed. The next pass should avoid another detector-chain micro-lever and target a deeper parser/dispatch primitive: carry a compact command metadata packet from RESP parsing into dispatch so command lookup, key extraction, ACL selector lookup, and runtime metadata refresh can share one canonical classification.
