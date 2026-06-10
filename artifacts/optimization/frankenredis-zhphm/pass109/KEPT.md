# Pass109 - kept bounded writer-pool handoff

Bead: `frankenredis-zhphm`

## Target

Profile-backed target: SET workload, pipeline 16, 50 clients. Pass108 moved
client writes off the event-loop thread with one writer thread per client, but
the post-keep profile still showed the output boundary and wake/queue overhead
as the next IO frontier.

This pass directly compares current `origin/main` (`fab34ce3f`) against a
bounded shared writer pool replacement for the already-encoded reply handoff.

## Lever

Replace per-client writer threads/channels with a fixed two-worker writer pool.
Each client owns at most one cloned writer stream in flight; completion returns
the stream and any unsent bytes to the event loop. Command parsing, command
execution, reply encoding, runtime/session mutation, replication ordering,
Pub/Sub ordering, MONITOR ordering, and blocked-client ordering remain on the
single event-loop thread.

## Isomorphism Proof

- Ordering: each client has at most one in-flight writer job. New output remains
  in that client's `write_buf` until the previous job completes, so later bytes
  cannot overtake earlier bytes. WouldBlock completions prefix unsent bytes back
  before newer buffered bytes.
- Command semantics: `Runtime` and `ClientSession` are still mutated only by the
  event-loop thread; worker threads only call `write` on already-encoded bytes.
- Output limits: `output_buffer_bytes`, recent output maxima, close cleanup, and
  hard-limit checks account for `write_buf + writer_in_flight_bytes`.
- Error behavior: writer failure marks the client closing and drains cleanup by
  the same disconnect path as a main-thread write error.
- Floating point and RNG: not touched.
- Golden transcript SHA-256 unchanged:
  `bdcab17275344bc6c13913945fd5cad11c800de1108c6f77776e16b1a09cc1f4`.

## Binaries

- Current baseline server:
  `0c411e0a397031d8f35e4960e0230b2b1d640a58738b408dd5a26b4edd0e41dc`
- Worker-pool candidate server:
  `18bf17729c64355286c8f2b54d2ca8d6b199c31c62981c9be1a9cdda00ee4603`

## Benchmarks

SET/P16/C50/1M, same `fr-bench` binary for both arms.

Paired current then worker-pool:

- Current mean: `1.12620941084s +/- 0.08320210031586027`
- Worker-pool mean: `0.8836509422399998s +/- 0.0926924170452931`
- Result: worker-pool `1.27x +/- 0.16` faster.

Reversed worker-pool then current:

- Worker-pool mean: `0.9813081227200001s +/- 0.03205508339610591`
- Current mean: `1.12881418112s +/- 0.0439369089955957`
- Result: worker-pool `1.15x +/- 0.06` faster.

Score: Impact 3.5 x Confidence 4 / Effort 3 = 4.67. Keep.

## Validation

- `cargo fmt -p fr-server --check`: pass.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_workerpool_check2 cargo check -p fr-server --all-targets`: pass. RCH fell open locally under worker pressure, but the command stayed crate-scoped.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_workerpool_clippy2 cargo clippy -p fr-server --all-targets --no-deps -- -D warnings`: pass. RCH fell open locally under worker pressure, but the command stayed crate-scoped.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_workerpool_tests cargo test -p fr-server -- --nocapture`: pass. RCH fell open locally under worker pressure, but the command stayed crate-scoped.
- `ubs crates/fr-server/src/main.rs`: UBS internal fmt/clippy/check/test subchecks pass; exit is nonzero from the existing broad heuristic inventory in this large server file.

## Reprofile

Candidate post-change perf profile:

- Workload: SET/P16/C50/3M.
- Throughput under perf: `1178056.8112407476 ops/sec`.
- Latency: p50 `637us`, p95 `972us`, p99 `1417us`, p999 `2327us`.
- Samples: 14,809, lost samples: 0.
- Main user-space self hotspots shifted to:
  - `fr_store::canonical_string_value_from_slice`: `3.27%`
  - `Store::set_plain_borrowed`: `3.04%`
  - `foldhash::quality::RandomState::hash_one::<&[u8]>`: `2.11%`
  - `ServerState::run_active_expire_cycle`: `1.61%`
  - `Runtime::plain_borrowed_default_key_write_allows`: `1.34%`

Next profile-backed route: replace/tighten the SET value/key path, especially
canonical value construction and default key write policy/hash work. Do not
repeat writer-boundary micro-tuning until a fresh profile puts it back on top.
