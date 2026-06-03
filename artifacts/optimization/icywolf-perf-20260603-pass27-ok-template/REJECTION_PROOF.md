# Pass 27 Rejection Proof: RESP OK Static Template

Bead: `frankenredis-nha7f`

## Profile Target

Post-pass26 SET pipeline=16 profiling had moved the server away from command
classification and into the response/socket path:

- `sendto`: 31,250 calls / 68.40% traced time
- `recvfrom`: 31,300 calls / 28.10% traced time
- GDB samples included `fr_protocol::RespFrame::encode_into` via
  `fr_server::encode_client_reply` on the `+OK\r\n` SET response stream.

No ready `[perf]` bead was available. `fr-server/src/main.rs` was also active
with the committed `frankenredis-apg7r` correctness fix, so this pass isolated a
non-overlapping `fr-protocol` lever.

## Lever Tested

One source lever only:

- For exactly `RespFrame::SimpleString("OK")`, append `b"+OK\r\n"` directly.
- All other simple strings continue through the CR/LF sanitizer path.
- RESP3 scalar encoding delegates to `encode_into`, so the same bytes apply
  after `HELLO 3`.

The source hunk was removed after the target benchmark rejected it.

## Baseline And Candidate

Focused rch-built harness:

- Baseline direct: `1.580224301s`, `126,564,311 ops/sec`
- Candidate direct: `0.733688611s`, `272,595,208 ops/sec`
- Baseline hyperfine: `1.584s +/- 0.040s`
- Candidate hyperfine: `716.0ms +/- 8.9ms`

Clean-worktree TCP A/B on top of `0bac982ff`:

- 500k SET p16 baseline: `1.680s +/- 0.035s`
- 500k SET p16 candidate: `1.649s +/- 0.038s`
- 2M SET p16 baseline: `7.347s +/- 0.191s`
- 2M SET p16 candidate: `7.645s +/- 0.136s`

The longer TCP run is the deciding target workload; it regressed by 4%.

## Isomorphism

- Golden RESP corpus SHA-256 matched:
  `ccd159c2b82a0f7ca5fe642971d543851e409e7c4006bcd8e5f7fa26b36ad650`.
- Byte order is unchanged: the candidate emitted the same five bytes in the
  same call site and did not reorder responses.
- Tie-breaking is unchanged: no map/set iteration or command selection changed.
- Floating point is not involved.
- RNG is not involved.
- Final production source retains no candidate hunk, so runtime behavior is
  exactly the current tree.

## Decision

Score is below `2.0` because the profiler-relevant TCP workload rejected the
lever despite a strong microbench win. No source was retained.

Next attack: do not keep micro-tuning `encode_into`. The deeper primitive is a
response segment queue / scatter-gather write path inspired by the alien
graveyard R2P2/Homa batching and `io_uring` submission-queue sections: preserve
Redis reply order while changing the write-side batching model.
