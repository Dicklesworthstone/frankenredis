# frankenredis-0s958 - Immediate readable-path flush

## Target

- Hotspot: fr-server pipelined write path.
- Profile signal: writev was rejected because SET pipeline=16 already emitted one send per 16-command batch; the retained hotspot was avoidable WRITABLE interest churn after reply coalescing.
- Lever: after `process_buffered_frames` coalesces replies, `handle_readable` attempts `ClientConnection::try_flush()` immediately when the per-client frame budget is not exhausted. WRITABLE is armed only for partial/WouldBlock writes.
- Build source for baseline: commit `58605c812` in `/data/projects/frankenredis-j05u5-baseline-1780357539`.
- Build source for after: current main worktree before commit.
- Build method: `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`.

## Benchmark

Command shape:

```text
fr-bench --host 127.0.0.1 --workload set --requests 50000 --clients 50 --pipeline 16 --datasize 3 --keyspace 10000
```

Hyperfine:

- warmup: 2
- runs: 5
- prepare: `FLUSHALL` before each run
- server mode: `strict`

Results:

| Build | Hyperfine mean | fr-bench ops/sec | p50 us | p95 us | p99 us |
| --- | ---: | ---: | ---: | ---: | ---: |
| before | 410.2 ms +/- 7.2 ms | 121,284.17 | 5,983 | 8,519 | 9,743 |
| after | 395.8 ms +/- 9.1 ms | 127,038.93 | 6,059 | 6,927 | 8,655 |

Delta:

- wall time: 3.5% lower
- throughput: 4.7% higher
- p95 latency: 18.7% lower
- p99 latency: 11.2% lower

Score: Impact 3 x Confidence 3 / Effort 2 = 4.5, keep.

## Isomorphism Proof

Deterministic RESP transcript:

```text
FLUSHALL
PING
SET j05u5:golden bar
GET j05u5:golden
```

SHA256:

```text
request: 0c310db326760bdae940bbc76356372ccc488b270bb89dd86e3e82280a33a13e
before output: 7bb6b41852c5de6128967ecb3e278117fb2baf1947a54c5a6d935155c53c321c
after output: 7bb6b41852c5de6128967ecb3e278117fb2baf1947a54c5a6d935155c53c321c
```

Behavior invariants:

- RESP reply bytes and ordering are unchanged for the golden transcript.
- Command dispatch ordering is unchanged: replies are still encoded in command order into the same per-client `write_buf`.
- Partial-write semantics are preserved: WRITABLE remains armed when `try_flush()` returns `Ok(false)`.
- Closing semantics are preserved: flush errors mark the connection closing and add the token to `closing_tokens`.
- Fairness guard is preserved: if `process_buffered_frames` exhausts the per-client frame budget, the old WRITABLE deferral path is used instead of immediate flushing.
- Existing write-interest cleanup is preserved for prior partial-write clients: if a token already had WRITABLE interest and the immediate flush drains it, registration returns to READABLE.
- No floating-point, RNG, hashing, or tie-breaking behavior is touched.

## Validation

- `cargo fmt --package fr-server --check`
- `rch exec -- cargo check -p fr-server --all-targets`
- `rch exec -- cargo clippy -p fr-server --all-targets -- -D warnings`
- `rch exec -- cargo test -p fr-server -- --nocapture`

Note: `rch exec -- cargo fmt` and arbitrary shell/hyperfine commands are rejected by rch here as non-compilation commands, so the runtime benchmark harness ran locally against rch-built binaries.
