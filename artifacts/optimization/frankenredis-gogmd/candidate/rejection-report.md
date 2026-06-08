# frankenredis-gogmd rejection report

## Target

- Bead: `frankenredis-gogmd`
- Lever: `GETSET` early key-index fast path in `fr_command::command_key_indexes`
- Workload: `getset-hit`, 300000 requests, 50 clients, pipeline 16, keyspace 10000, datasize 3
- Baseline binary: `/tmp/codex-fr-gogmd-baseline-target/release-perf/frankenredis`
- Candidate binary: `/tmp/codex-fr-gogmd-candidate-target/release-perf/frankenredis`

## Profile basis

Fresh baseline profile:

- `fr_protocol::parse_command_args_borrowed_into`: 0.78% self
- `<fr_runtime::Runtime>::execute_frame_internal`: 0.69% self
- `fr_command::command_table_index`: 0.62% self
- `<fr_runtime::Runtime>::refresh_store_runtime_info_context`: 0.89% self

The candidate tried to avoid the generic UTF-8/table lookup path in `command_key_indexes` for the profiled `GETSET` command. `GETSET` has a static key index of `1` when at least one argument exists, matching the generic command-table path.

## Behavior proof

- Raw RESP golden comparison: equal
- Baseline SHA-256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- Candidate SHA-256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-gogmd-check cargo check -p fr-command --all-targets`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-gogmd-getset-test cargo test -p fr-command hget_and_getset_command_metadata_fast_paths_preserve_behavior -- --nocapture`: 1 passed
- `cargo fmt -p fr-command -- --check`: failed on pre-existing formatting drift outside this one-line hunk

Isomorphism: the candidate changed only key-index discovery for `GETSET`, returning the same `[1]` or empty vector as the generic command-table route. Reply bytes, command ordering, tie-breaking, floating point, RNG, persistence, replication, and error classes are unchanged.

## Benchmark result

Baseline one-sided hyperfine:

- Baseline: `2.15672168378s +/- 0.02303234347s`

Paired same-host hyperfine:

- Baseline: `2.1612674338s +/- 0.02524385139s`
- Candidate: `2.3210490680s +/- 0.11537413223s`
- Hyperfine summary: baseline ran `1.07 +/- 0.05x` faster than candidate

## Decision

Rejected. Score: `0.0` because the candidate was slower on the profiled workload and does not meet the Score >= 2.0 keep threshold.

The production and test hunks were removed. This confirms that one-command key-index micro-fast paths are the wrong family. The next pass should attack a deeper primitive: carry or cache command metadata across runtime ACL, key extraction, dispatch, and command stats so the whole metadata band is removed rather than one command's table lookup.
