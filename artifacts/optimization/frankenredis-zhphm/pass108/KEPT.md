# Pass108 - kept owned client writer boundary

Bead: `frankenredis-zhphm.5`

## Target

Profile-backed target: SET workload, pipeline 16, 50 clients. Baseline profile
kept showing the hot boundary in server-side socket writes:

- `ClientConnection::try_flush`: 50.94% children in pass5 current-main profile.
- pass108 baseline profile: `__send` 45.70% children, `handle_readable`
  11.11% children, `fr_store::canonical_string_value_from_slice` 17.93% self.

## Lever

Accepted client connections now get an owned bounded writer channel/thread for
already-encoded reply bytes. Command parsing, command execution, reply encoding,
runtime/session ownership, replication ordering, Pub/Sub ordering, MONITOR
ordering, and blocked-client ordering stay on the main event-loop thread.

The main loop appends bytes to the per-client `write_buf` exactly as before,
then transfers the coalesced bytes to that client's FIFO writer queue. Production
writer-backed clients do not write directly from the main loop, so later bytes
cannot overtake earlier queued bytes. The existing nonblocking `try_flush` path
remains the fallback for test and non-writer connections.

## Isomorphism Proof

- Ordering: one FIFO queue per client; all producers (`handle_readable`,
  replication fanout, Pub/Sub, MONITOR, unblocked clients, deferred clients)
  arm through the same writer handoff when a writer exists.
- Command semantics: `Runtime` and `ClientSession` mutation remain serial on
  the event-loop thread; no command execution moved to worker threads.
- Output limits: `output_buffer_bytes`, recent output maxima, close cleanup, and
  hard-limit checks count `write_buf + queued writer bytes`.
- Error behavior: writer failure marks the client closing and drops pending
  output, matching the old write-error disconnect behavior.
- Floating point and RNG: not touched.
- Golden transcript: raw RESP response SHA-256 unchanged:
  `bdcab17275344bc6c13913945fd5cad11c800de1108c6f77776e16b1a09cc1f4`.

## Binaries

- Baseline server:
  `be2cf65781baff59aa6d4455520608b04e03251f56a98c28c1f67536caa49f4b`
- Candidate server:
  `1c99d430f1c0aed09d4f11932f7117e2b7284fa53084ffb9d0e61b816b127811`

## Benchmarks

SET/P16/C50/1M, same `fr-bench` binary for both arms.

Paired baseline then candidate:

- Baseline mean: `1.2924742568s +/- 0.0495206843`
- Candidate mean: `1.1239187264s +/- 0.0324472277`
- Result: candidate `1.15x +/- 0.06` faster.

Reversed candidate then baseline:

- Candidate mean: `1.1061290835s +/- 0.0715960871`
- Baseline mean: `1.2782567493s +/- 0.0997343861`
- Result: candidate `1.16x +/- 0.12` faster.

Post-rebase current-base confirmation after rebasing onto `origin/main`
`75e9444f8`:

- Current `origin/main` baseline server:
  `c7c361cf256f51d3fa99d4af98ba7279084ff7599666d9a3164a15eab8063b2d`
- Rebased candidate server:
  `de28d83efbe378759f8f18aae75a1634e6d2ceff546a4cb58c3564e24dad220d`
- Paired: baseline `1.314s +/- 0.110`, candidate
  `1.149s +/- 0.082`, candidate `1.14x +/- 0.13` faster.
- Reversed: candidate `1.085s +/- 0.057`, baseline
  `1.254s +/- 0.038`, candidate `1.16x +/- 0.07` faster.

Score: Impact 3.5 x Confidence 4 / Effort 3 = 4.67. Keep.

## Validation

- `cargo fmt -p fr-server --check`: pass.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p108_check2 cargo check -p fr-server --all-targets`: pass.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p108_test cargo test -p fr-server -- --nocapture`: pass. The job fell back local after `rch` queue timeout, but stayed crate-scoped.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p108_clippy_nodeps2 cargo clippy -p fr-server --all-targets --no-deps -- -D warnings`: pass.
- Post-rebase `cargo fmt -p fr-server --check`: pass.
- Post-rebase `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p108_rebased_check cargo check -p fr-server --all-targets`: pass on `vmi1227854`.
- Post-rebase `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p108_rebased_test cargo test -p fr-server -- --nocapture`: pass. `rch` fell back local after `queue_timeout`, but stayed crate-scoped.
- Post-rebase `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p108_rebased_clippy_nodeps cargo clippy -p fr-server --all-targets --no-deps -- -D warnings`: pass on `ovh-a`.
- Full local-dependency clippy command for `fr-server` is currently blocked by
  unrelated `fr-store` `clippy::doc_lazy_continuation` warnings at
  `crates/fr-store/src/lib.rs:1737`, `1738`, and `1793`.

## Reprofile

Candidate post-change perf profile:

- Workload: SET/P16/C50/3M.
- Throughput under perf: `974950.7427 ops/sec`.
- Latency: p50 `770us`, p95 `1072us`, p99 `1330us`.
- Samples: 25K, lost samples: 0.
- New hot area: writer threads own the `__send` cost; main-loop self hotspots
  shift to `fr_store::canonical_string_value_from_slice` (~9.95% self),
  `handle_readable` / `__libc_recv`, `parse_command_args_borrowed_into`, and
  queue/waker overhead.

Next pass should not repeat writable deferral. Profile-backed next targets are
batching writer wakeups/chunks, read/parse ownership boundary, or the hot
canonical string value path.
