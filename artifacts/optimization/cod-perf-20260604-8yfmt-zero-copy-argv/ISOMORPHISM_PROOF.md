# frankenredis-8yfmt isomorphism proof

## Change

Move parsed command-frame argument buffers into a single argv on the TCP hot path,
dispatch through `Runtime::execute_argv_with_unix_time_us(&argv, ...)`, and reuse
that same argv slice for QUIT, blocking-command, subscription-mode, reply
suppression, and replication follow-up checks.

## Profile-backed target

The bead's profile note identifies `frame_to_argv` allocator samples as the
dominant request-path cost. The prior partial candidate was rejected because it
still rebuilt argv in server-side post-dispatch helpers. This candidate completes
the one lever by making those helpers argv-only.

## Benchmark

Controlled SET p16 run, 50 clients, 500000 requests, 10000-key keyspace, 3-byte
payload. Both runs used the same rch-built `fr-bench` binary; only the server
binary changed.

- Baseline hyperfine mean: 1.726355555565 s
- Candidate hyperfine mean: 1.672385829275 s
- Speedup: 1.0322710975812304x
- Baseline last throughput: 300904.04556287616 ops/sec
- Candidate last throughput: 309862.9596975812 ops/sec
- Last-sample throughput delta: +2.97733256392294%
- Baseline last p99: 4611 us
- Candidate last p99: 3969 us
- Last-sample p99 delta: -13.923227065712426%

Score: Impact 2.0 * Confidence 4.0 / Effort 3.0 = 2.67.

## Isomorphism

- Ordering preserved: yes. The parser still emits command arguments in original
  array order; the change only moves `Vec<u8>` buffers instead of cloning them.
- Tie-breaking unchanged: yes. Command dispatch, blocking resolution, replica
  follow-up, and subscription gates receive the same argv bytes in the same order.
- Floating-point: N/A. This lever does not alter numeric algorithms or timeout
  parsing logic.
- RNG seeds: N/A. This lever does not add randomness.
- Error classes preserved: yes. `*0` and `*-1` continue to no-op before argv
  materialization. Malformed command frames that cannot be moved into argv still
  take the existing frame-based runtime error path.
- Threat digests preserved: yes. Frame-owned callers still hash the original
  `RespFrame`; the server argv path reconstructs the canonical bulk-string
  command frame only on cold threat-evidence paths.

## Golden sha256

- `golden-fixtures.sha256`: 210 fixture/golden files under
  `crates/fr-conformance/fixtures`
- `golden-fixtures.sha256check.txt`: `sha256sum -c` passed for all entries
- `artifacts.sha256`: benchmark/proof artifact manifest
- `artifacts.sha256check.txt`: `sha256sum -c` passed for all entries

## Validation Commands

```text
env CARGO_TARGET_DIR=target-cod-8yfmt-candidate-rch rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench
hyperfine --warmup 2 --runs 8 --show-output '<fr-bench baseline command>' --export-json baseline-hyperfine.json
hyperfine --warmup 2 --runs 8 --show-output '<fr-bench candidate command>' --export-json candidate-hyperfine.json
sha256sum -c artifacts/optimization/cod-perf-20260604-8yfmt-zero-copy-argv/golden-fixtures.sha256
sha256sum -c artifacts/optimization/cod-perf-20260604-8yfmt-zero-copy-argv/artifacts.sha256
```
