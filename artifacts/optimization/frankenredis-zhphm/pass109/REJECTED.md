# Pass109 - rejected read/parse ownership boundary

Bead: `frankenredis-zhphm.6`

## Target

After pass108 moved the dominant socket write cost to per-client writer threads,
this pass re-profiled the SET/P16/C50 frontier before attempting any safe
read/parse ownership handoff.

Baseline:

- Command: `fr-bench` SET, 50 clients, pipeline 16, 1,000,000 requests.
- Hyperfine: `1.227962603s +/- 0.249920207s`, median `1.133711212s`,
  range `1.053621887s..1.787707027s`.
- Profile run: SET/P16/C50/3,000,000 under `perf record -F 499`.
- Throughput under perf: `765358.2498 ops/sec`.
- Latency under perf: p50 `950us`, p95 `1519us`, p99 `2113us`, p999 `3431us`.
- Perf samples: `19K`, lost samples `0`.

## Profile Evidence

Top self rows on the server-side profile:

- `fr_store::canonical_string_value_from_slice`: `7.35%`.
- `<fr_store::Store>::set_plain_borrowed`: `5.24%`.
- `fr_protocol::parse_command_args_borrowed_into`: `1.22%`.
- `<fr_runtime::Runtime>::refresh_client_memory_aggregates`: `1.16%`.
- `frankenredis::process_buffered_frames`: `0.65%`.
- `frankenredis::handle_readable`: `0.26%`.
- `core::str::from_utf8`: `0.24%`.
- `<fr_protocol::RespFrame>::encode_into`: `0.19%`.

The measured read/parse surface is real but secondary. A bounded read-worker or
parse-worker handoff would add queueing, wakeups, ownership transfer, and
ordering proof burden to chase roughly low-single-digit visible CPU in this
workload, while the profile now points more strongly at store value
canonicalization and command metadata/layout.

## Candidate Tried

A narrower all-safe candidate was inspected and locally trialed: reuse the
borrowed argv slice vector across frames in `process_buffered_frames` instead
of allocating `Vec<&[u8]>` for every strict multibulk frame.

The candidate failed the compile gate. Holding a reusable `Vec<&[u8]>` across
event-loop iterations widened the immutable borrow of `conn.read_buf`; the
crate-scoped check rejected later mutable borrows of `conn` with `E0502`
(`cannot borrow *conn as mutable because it is also borrowed as immutable`).
Explicit `clear()` calls at the parse-block boundary and loop boundary did not
make the lifetime narrow enough for safe Rust.

No production source change was retained.

## Isomorphism Proof

- Ordering: unchanged. Command parsing, execution, blocking, pause handling,
  `MAX_FRAMES_PER_CLIENT_TICK`, `consumed_total`, and final `read_buf.drain`
  behavior remain exactly on the baseline code path.
- Tie-breaking: unchanged. No key ordering, set/zset ordering, or command
  matching order changed.
- Floating point: unchanged. No numeric parser or score path changed.
- RNG: unchanged. No random-key, SPOP, LFU, or hash-seed path changed.
- Golden output: no candidate binary exists because the only code candidate
  failed `cargo check` and was removed. The working tree has no retained
  production diff for `crates/fr-server/src/main.rs`.

## Decision

Reject. Score estimate for read/parse offload at this profile point is
Impact `1.0` x Confidence `2.0` / Effort `4.0` = `0.5`, below the `2.0`
keep gate. Score estimate for the borrowed-argv scratch reuse candidate is
`0` because it fails the compile gate.

## Next Route

Pass110 should attack a different command-packet/layout primitive that avoids
storing borrowed byte slices across event-loop state. Viable profile-backed
directions:

- parse argv into reusable offset/length metadata rather than `&[u8]` slices;
- fuse the strict SET command packet path with store canonicalization;
- re-profile and attack `fr_store::canonical_string_value_from_slice` if it
  remains the highest self row.

Alien-graveyard match: tail-latency decomposition keeps the IO boundary honest,
but the next productive primitive is a data-plane representation change rather
than a read-thread handoff.
