# frankenredis-ohsk5.8 clmemlazy rejection proof

## Target

Profile-backed target under `frankenredis-ohsk5`: repeated P16 profiles showed
`Runtime::refresh_client_memory_aggregates` in the hot path. Recent examples:

- `artifacts/optimization/frankenredis-gnwuw/post102-profile/set-p16-1m-perf-report.txt`:
  `0.75%` self on SET P16/1M after the borrowed SET in-place keep.
- Older GET/SET profiles showed the same function in the `0.5%` to `1.5%`
  range.

## Candidate

One lever tested: mark client memory aggregates dirty on session
add/update/remove and recompute only when `MEMORY STATS` or `INFO memory`
consumes the values.

The source hunk was removed after the benchmark gate failed.

## Behavior proof while candidate binary was applied

- Ordering preserved: no command ordering, client FIFO, or replication/AOF
  ordering was changed.
- Tie-breaking unchanged: no sorted-set, scan, key-order, or comparison logic was
  changed.
- Floating-point: N/A.
- RNG/hash seed: unchanged.
- Golden RESP transcript:
  - baseline SHA-256:
    `88e0947c283e4236deab7ed211d7dc5179f98c16017177d00e2ed5e09dbcf7c8`
  - candidate SHA-256:
    `88e0947c283e4236deab7ed211d7dc5179f98c16017177d00e2ed5e09dbcf7c8`
  - `cmp` passed byte-for-byte.

## Build notes

- Candidate release-perf build passed with:
  `rch exec -- env CARGO_TARGET_DIR=/tmp/tealotter-fr-clmemlazy-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`
  RCH had no admissible workers and fell back locally.
- Baseline remote RCH build initially failed because the scratch worktree lacked
  the local `legacy_redis_code` command metadata directory. Added a scratch
  symlink to the shared oracle directory and rebuilt locally with the same
  narrow package set:
  `env CARGO_TARGET_DIR=/tmp/tealotter-fr-clmemlazy-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench`.

## Benchmarks

Baseline-only SET P16/300k:

- baseline `483.6 ms +/- 43.1 ms`

Paired SET P16/300k:

- baseline `487.7 ms +/- 20.3 ms`
- candidate `479.1 ms +/- 24.2 ms`
- candidate `1.02x +/- 0.07` faster

Reversed SET P16/300k:

- candidate `517.6 ms +/- 57.0 ms`
- baseline `685.5 ms +/- 108.0 ms`
- candidate `1.32x +/- 0.25` faster, too noisy to keep without confirmation

Longer SET P16/1M confirmation:

- baseline `1.375 s +/- 0.022 s`
- candidate `1.422 s +/- 0.066 s`
- baseline `1.03x +/- 0.05` faster

## Decision

Rejected under the Score>=2.0 keep gate. The only strong-looking 300k result
did not survive the longer 1M confirmation, where baseline was faster. Score:
Impact `0` x Confidence `4` / Effort `1` = `0`.

Next route: stop client-memory aggregate laziness and re-profile the current
`zhphm`/P16 IO frontier before any safe read/parse offload work.
