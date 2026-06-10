# Pass121 Rejection: Direct SET Packet Sub-Batch

Bead: `frankenredis-ohsk5.18`
Base: `c2586a460`

## Target

Pass121 reprofiled current main after the pass120 response-ring rejection.

- Baseline SET/P16/C50/1M: `679.4 ms +/- 21.0 ms`.
- Server-only SET/P16/C50/3M: `1687913.7906943588 ops/sec`, p50 `429us`,
  p95 `676us`, p99 `897us`, p999 `1851us`, 0 lost perf samples.
- Top flat profile rows:
  - `fr_store::canonical_string_value_from_slice`: `4.63%` self.
  - `process_buffered_frames`: `2.27%` self.
  - unresolved kernel receive path: `2.10%` self.
  - `[vdso]` time path: `1.88%` self.
  - `Runtime::plain_borrowed_default_key_write_allows`: `1.41%` self.
  - `Runtime::execute_plain_set_borrowed`: `1.17%` self.
  - `Store::set_plain_borrowed`: `1.00%` self.

The route avoided previously rejected direct SET gate caching, static OK,
one-probe SET/drop_if_expired, wake coalescing, worker fanout, tiny sync flush,
output-buffer capacity reuse, and integer/canonicalization micro-families.

## Lever Tested

Candidate: when the server loop sees a direct borrowed `SET` packet, process
consecutive direct SET packets in a tight sub-batch before returning to the
generic borrowed-argv path.

The candidate still used `Runtime::execute_plain_set_borrowed` for every SET
and still encoded each returned `RespFrame` normally. It only changed the
server-loop control flow around consecutive direct SET packets.

The rejected source hunk is retained at:

- `artifacts/optimization/frankenredis-ohsk5/pass121/candidate/rejected-source-hunk.patch`

The production source hunk was removed after the benchmark gate failed.

## Isomorphism Proof

Baseline and candidate TCP transcripts matched byte-for-byte.

- Request sha256:
  `f8005a59ce8c45b8eca01efaf6b042967d05fa9f2688eb1b7dccd7ac850f9d68`
- Golden sha256:
  `447297a82ed8f8aa3b76483daa451bf4cb5c309b63200687da7f93084ffc5e32`
- Response bytes: `536`

The golden request prepended two consecutive direct SET packets and a PING to
the pass119 SET/object/digest corpus, so the sub-batch path was covered. The
proof preserves raw RESP bytes, per-client ordering, CLIENT REPLY behavior,
expiry/order, keyspace side effects, object encoding, DEBUG DIGEST behavior,
floating-point behavior, and RNG behavior.

## Validation Run On Candidate

- `rustfmt --edition 2024 --check crates/fr-server/src/main.rs`: passed.
- `cargo test -p fr-server process_buffered_frames_sub_batches_consecutive_direct_sets -- --nocapture`:
  passed via `rch exec` local fallback from the detached `/tmp` worktree.
- `cargo build -p fr-server -p fr-bench --profile release-perf`: passed via
  `rch exec` local fallback from the detached `/tmp` worktree.

## Performance

Paired SET/P16/C50/1M:

- Baseline: `680.7 ms +/- 36.6 ms`.
- Candidate: `652.4 ms +/- 37.6 ms`.
- Candidate was `1.04x +/- 0.08` faster.

Reversed SET/P16/C50/1M:

- Candidate: `685.0 ms +/- 35.9 ms`.
- Baseline: `646.4 ms +/- 25.9 ms`.
- Baseline was `1.06x +/- 0.07` faster.

## Decision

Reject. The effect flips by order and is nowhere near the `>=1.20x` target.
The source tree is restored to no-diff state; only this rejection bundle is
retained.

Score: `1.0 < 2.0`.

Next route should stop direct SET packet-control microlevers and attack a
deeper primitive: store value representation/layout, command timing source, or
a broader zero-copy output model backed by a fresh cross-workload profile.
