# frankenredis-zhphm.4 pass5 rejection

Target: current-main IO boundary under SET/P16/C50. The pass4 pub/sub drain
guard was rejected, so this pass routed into the parent structural primitive:
Redis-style safe-Rust IO-thread/read-parse/write-encode offload. Before editing,
current `main` was rebuilt and profiled.

Decision: rejected. Production source hunks were removed before commit.

## Alien Primitive Match

- Graveyard match: share-nothing/thread-per-core batching, bounded queues with
  backpressure, zero-copy ownership transfer, and async I/O submission-queue
  principles adapted to safe Rust without `io_uring` unsafe bindings.
- Trial lever: defer readable-handler flushes to the existing writable path.
  This tested whether moving `send()` out of `handle_readable` improved
  multi-client command progress before committing to a full writer-thread
  boundary.

## Current-Main Baseline

- Build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p5_baseline cargo build --profile release-perf -p fr-server -p fr-bench`
- Worker: `vmi1227854`
- Benchmark: SET/P16/C50/1M
- Hyperfine mean: `1.18852607395s`
- Hyperfine stddev: `0.04592703512024633s`
- Artifact: `baseline/baseline-set-p16-1m-hyperfine.json`

## Current-Main Profile

- Profile workload: SET/P16/C50/3M
- Ops/sec: `941383.1395538398`
- Samples: `3K`, lost samples: `0`
- Top relevant frames:
  - `ClientConnection::try_flush`: `50.94%` children, `0.06%` self
  - `handle_readable`: `11.51%` children, `0.17%` self
  - `Runtime::execute_plain_set_borrowed`: `7.39%` children, `1.26%` self
  - `process_buffered_frames`: `3.86%` children, `1.79%` self
- Artifact: `profile/perf-set-p16-c50-report.txt`

## Candidate

- Build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p5_candidate cargo build --profile release-perf -p fr-server -p fr-bench`
- Worker: `ovh-a`
- Lever: always register `WRITABLE` for pending output after `handle_readable`;
  let `handle_writable` perform `try_flush`.
- Standalone candidate mean: `1.2239817387050003s`
- Standalone candidate stddev: `0.03352024801540714s`
- Artifact: `candidate/candidate-set-p16-1m-hyperfine.json`

## Paired Timing

- Baseline mean: `1.08672796053s +/- 0.03625828709488476s`
- Candidate mean: `1.2546403115299998s +/- 0.05377419157144022s`
- Hyperfine summary: baseline `1.15x +/- 0.06` faster
- Artifact: `paired/paired-set-p16-1m-hyperfine.json`

## Reversed Timing

- Candidate mean: `1.2818711688450002s +/- 0.03740523376848125s`
- Baseline mean: `1.089070163595s +/- 0.03882920212538736s`
- Hyperfine summary: baseline `1.18x +/- 0.05` faster
- Artifact: `reversed/reversed-set-p16-1m-hyperfine.json`

## Behavior Proof

- Transcript SHA-256: `dffd93d857ab070c733ec46d61274b935b4f85fd05992abbf4f979e6959fdf67`
- Baseline response SHA-256: `bdcab17275344bc6c13913945fd5cad11c800de1108c6f77776e16b1a09cc1f4`
- Candidate response SHA-256: `bdcab17275344bc6c13913945fd5cad11c800de1108c6f77776e16b1a09cc1f4`
- Ordering: per-client `write_buf` order stayed unchanged; only flush scheduling moved.
- Tie-breaking, floating point, RNG: not touched.

## Validation

- `cargo fmt -p fr-server --check`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p5_check cargo check -p fr-server --all-targets`: passed
- Golden TCP transcript: byte-identical baseline/candidate responses

## Score

- Impact: `-1.0`
- Confidence: `0.95`
- Effort: `1.0`
- Score: rejected, below the `2.0` keep threshold.

## Next Target

Do not repeat writable-event deferral. The profile says `send()` itself is the
dominant cost, and the existing immediate flush avoids a costly extra event-loop
turn. The next structural pass should attack the real alien primitive: an owned
writer boundary with bounded queues/backpressure, where command execution stays
serial but write syscall work can proceed outside the command-processing loop.
