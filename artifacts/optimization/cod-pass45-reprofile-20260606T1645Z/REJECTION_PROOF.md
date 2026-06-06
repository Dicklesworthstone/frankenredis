# Pass45 Rejection Proof: Parser Byte-Range Scratch Reuse

Target bead: `frankenredis-ohsk5`

## Profile-Backed Target

Current pushed main `824639995`, SET P16/1M profile:

- Throughput: `418826.66 ops/sec`
- p99: `3239 us`
- Samples: `19042`, lost `0`
- Top flat symbols: `Store::drop_if_expired` `7.38%`,
  `__memcmp_avx2_movbe` `6.56%`, `Runtime::execute_dispatch` `6.06%`,
  time reads `4.04%`, `Runtime::execute_frame_internal` `3.25%`,
  `mi_free` `3.23%`, `dispatch_with_client_context` `2.57%`,
  `process_buffered_frames` `1.53%`,
  `parse_command_args_borrowed_into` `1.35%`.

## Lever Tested

Add a strict multibulk parser variant that stores argument byte ranges into
caller-reused scratch, then reuse that range scratch across
`process_buffered_frames` iterations before copying into the existing owned
`Vec<Vec<u8>>` dispatch arena.

This kept the existing dispatch ownership model and only changed parser/server
scratch metadata while the candidate was applied.

## Behavior Proof

Raw TCP RESP transcript covered:

- `PING`
- `SET` / `GET`
- binary bulk value with embedded CRLF bytes
- `DEL`
- `QUIT`

Baseline and candidate replies matched exactly:

- Reply bytes: `54` baseline, `54` candidate
- Reply SHA-256:
  `dfb555a4a8a67b66ee78296c231deb8ef024426befb1132411582fd2c6e6bb3f`
- Input SHA-256:
  `09000457d9d24c2632b10129f2bdcd7f442ccf16e8f9ce82221e2d34090135b6`
- `cmp` passed.

Isomorphism:

- Ordering preserved: yes; the parser emits ranges in RESP array order and the
  server dispatch loop consumed the same number of bytes before dispatch.
- Tie-breaking unchanged: N/A; no sorted ordering path changed.
- Floating-point unchanged: N/A.
- RNG unchanged: N/A.
- Parser cursor semantics preserved: yes; bulk parsing intentionally preserves
  the command-path behavior of advancing past trailing two bytes without
  validating them, matching the existing borrowed parser.

## Verification While Candidate Was Applied

- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass45-parser-range-check cargo check -p fr-protocol -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass45-parser-range-tests cargo test -p fr-protocol parse_command_arg_ranges_into -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass45-server-tests cargo test -p fr-server process_buffered_frames -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass45-parser-range-candidate cargo build --release -p fr-server -p fr-bench`

`cargo fmt -p fr-protocol -p fr-server -- --check` remains red because
`fr-protocol` has pre-existing rustfmt drift in the `fpconv` table/function
formatting. No broad formatting churn was retained.

## Benchmark Decision

Fresh baseline hyperfine before candidate:

- SET P16/300k: `1.10675248642 s +/- 0.0814984609675006`

Candidate direct run:

- SET P16/300k: `391830.47 ops/sec`, p99 `3689 us`

The initial paired run used a stale baseline binary from before peer replication
commits changed `fr-runtime`, `fr-command`, and `fr-server`, so it is retained
only as superseded evidence:

- Baseline: `1.00738096844 s +/- 0.03120246971259863`
- Candidate: `1.10004165394 s +/- 0.10696099883964477`
- Result: baseline ran `1.09x` faster than candidate.

Fair current-source paired hyperfine after rebuilding baseline from the current
source without the candidate:

- Baseline: `0.96927005486 s +/- 0.011778335384915834`
- Candidate: `0.96970703976 s +/- 0.013338231538178098`
- Result: baseline ran only `1.00x +/- 0.02x` faster than candidate.

Decision: reject under Score >= 2.0 because the fair current-source comparison
produced no measurable win. The production source hunk was removed.

Next primitive: stop parser scratch metadata. Attack zero-copy RESP frame
routing into a borrowed dispatch API that removes the `Vec<Vec<u8>>` argv copy
as a class, target ratio `>=1.20x` on SET P16, or attack a reply
arena/write-batching primitive only if a fresh profile shifts output higher.
