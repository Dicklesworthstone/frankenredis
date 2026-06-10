# Pass119 Rejection: Cached Direct SET Write Gate

Bead: `frankenredis-ohsk5.15`
Base: `bc80dd725`

## Target

Pass119 reprofiled current main before editing.

- Baseline SET/P16/C50/1M: `682.1 ms +/- 32.9 ms`.
- Server-only SET/P16/C50/3M: `1659548.4218610302 ops/sec`, p50 `435us`,
  p95 `686us`, p99 `910us`, p999 `1539us`, 0 lost perf samples.
- Top flat profile rows:
  - `fr_store::canonical_string_value_from_slice`: `3.20%` self.
  - unresolved kernel receive path under `handle_readable`: `2.55%` self.
  - `[vdso]`/`clock_gettime` under `execute_plain_set_borrowed`: `2.22%` self.
  - `Runtime::plain_borrowed_default_key_write_allows`: `1.66%` self.
  - `Store::set_plain_borrowed`: `1.56%` self.
  - `process_buffered_frames`: `1.38%` self.

Attaching `strace -f -c` to the running server was blocked by ptrace policy.
Launching the server under `strace -f -c` completed the 300k benchmark but the
diagnostic wrapper did not exit cleanly enough to write a syscall summary, so
the retained profile evidence is perf-first.

## Lever Tested

Candidate: cache the default borrowed-write gate once per buffered processing
pass and add a runtime entry point for direct SET packets that were already
validated by `parse_borrowed_plain_set_packet`.

This removed duplicate parser limit checks and repeated default write-gate work
from the exact direct SET packet path, while resetting the cached decision
before any generic parsed path. The generic borrowed-args SET path still used the
full `execute_plain_set_borrowed` validation.

The rejected source hunk is retained at:

- `artifacts/optimization/frankenredis-ohsk5/pass119/candidate/rejected-source-hunk.patch`

The production source hunk was removed after the benchmark gate failed.

## Isomorphism Proof

Baseline and candidate TCP transcripts matched byte-for-byte.

- Golden sha256:
  `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`.

The proof covers ordinary SET, GET, GETSET, DEL, MSET, MGET, INCR, GETDEL, and
raw RESP framing. The rejected no-diff state leaves per-client ordering,
CLIENT REPLY behavior, expiry/order, keyspace side effects, object encoding,
DEBUG DIGEST behavior, floating-point behavior, RNG behavior, and reply bytes
unchanged.

## Validation Run On Candidate

- `cargo fmt -p fr-runtime -p fr-server -- --check`: blocked by pre-existing
  broad `fr-runtime/src/lib.rs` rustfmt drift outside the candidate hunk.
- `cargo check -p fr-runtime -p fr-server --all-targets`: passed on RCH worker
  `vmi1227854`, with one pre-existing unrelated test warning.
- `cargo test -p fr-runtime validated_plain_set_borrowed -- --nocapture`:
  passed via RCH local fallback, with the same pre-existing unrelated warning.
- `cargo clippy -p fr-runtime -p fr-server --lib --bins --no-deps -- -D warnings`:
  passed on RCH worker `vmi1227854`.
- `cargo build -p fr-server -p fr-bench --profile release-perf`: passed via RCH
  local fallback.

## Performance

Paired SET/P16/C50/1M:

- Baseline: `659.9 ms +/- 27.1 ms`.
- Candidate: `641.5 ms +/- 28.6 ms`.
- Candidate was `1.03x +/- 0.06` faster.

Reversed SET/P16/C50/1M:

- Candidate: `662.0 ms +/- 35.4 ms`.
- Baseline: `679.9 ms +/- 20.3 ms`.
- Candidate was `1.03x +/- 0.06` faster.

## Decision

Reject. Both orders show the same small directional result, but the uncertainty
still includes no real win and the effect is far below the pass119 target for a
structurally meaningful primitive.

Score: `1.5 < 2.0`.

## Next Route

Do not repeat direct SET parser gate caching, static OK replies, one-probe
`SET`/`drop_if_expired`, wake coalescing, worker-count fanout, tiny synchronous
flush, or integer/canonicalization micro-families.

The next pass should reprofile current main and move to a larger primitive:
broader output job granularity, response-ring ownership, command-batch arena
execution, or a different zero-copy parser/output path with a target ratio of
`>=1.20x`.
