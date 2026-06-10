# Pass120 Rejection: Drained Writer Buffer Recycling

Bead: `frankenredis-ohsk5.17`
Base: `7100b896c`

## Target

Pass120 reprofiled current main after the direct SET gate-cache rejection.

- Baseline SET/P16/C50/1M: `684.7 ms +/- 9.5 ms`.
- Server-only SET/P16/C50/3M: `1691982.397758914 ops/sec`, p50 `425us`,
  p95 `681us`, p99 `984us`, p999 `2265us`, 0 lost perf samples.
- Top flat profile rows:
  - `fr_store::canonical_string_value_from_slice`: `3.08%` self.
  - unresolved kernel receive path under `handle_readable`: `2.68%` self.
  - `[vdso]`/time path under borrowed SET: `2.23%` self.
  - `process_buffered_frames`: `2.19%` self.
  - `Store::set_plain_borrowed`: `1.13%` self.
  - `Runtime::plain_borrowed_default_key_write_allows`: `1.07%` self.

The bead explicitly ruled out another direct SET gate, static OK, one-probe
SET/drop_if_expired, wake coalescing, tiny synchronous flush, or
integer/canonicalization micro-family. The tested route therefore targeted
response-ring ownership in the writer/output path.

## Lever Tested

Candidate: when a writer thread fully drained a client output job, return that
now-empty `Vec<u8>` to the connection as its `write_buf` capacity if no newer
output had accumulated meanwhile.

This keeps per-client reply ordering, response bytes, CLIENT REPLY suppression,
and socket write state unchanged. It only changes ownership of an empty buffer
after the worker has already reported `Drained`.

The rejected source hunk is retained at:

- `artifacts/optimization/frankenredis-ohsk5/pass120/candidate/rejected-source-hunk.patch`

The production source hunk was removed after the benchmark gate failed.

## Isomorphism Proof

Baseline and candidate TCP transcripts matched byte-for-byte.

- Request sha256:
  `14a42bc4c9c744ee28ca89f4167c42309f4acff12743e030ab13efa9fc111b36`
- Golden sha256:
  `ed6eb02107fc024f5a903539daaaac99d2123b8b194fdcbf0cd7cdefa305230a`
- Response bytes: `519`

The proof reuses the pass119 SET/integer-object encoding corpus covering SET,
GET, OBJECT ENCODING/REFCOUNT, DEBUG DIGEST, INCRBY, INCR, DECR, and raw RESP
framing. Since the candidate only recycled empty output capacity, expiry/order,
keyspace side effects, object encoding, DEBUG DIGEST behavior, floating-point
behavior, RNG behavior, CLIENT REPLY semantics, and per-client ordering remain
unchanged.

## Validation Run On Candidate

- `rustfmt --edition 2024 --check crates/fr-server/src/main.rs`: passed.
- `cargo fmt --check`: blocked by pre-existing workspace rustfmt drift outside
  the candidate hunk.
- `cargo test -p fr-server drained_writer_buffer_reuse_preserves_pending_output -- --nocapture`:
  passed via `rch exec` local fallback from the detached `/tmp` worktree.
- `cargo build -p fr-server -p fr-bench --profile release-perf`: passed via
  `rch exec` local fallback from the detached `/tmp` worktree.

## Performance

Paired SET/P16/C50/1M:

- Baseline: `735.1 ms +/- 22.5 ms`.
- Candidate: `662.5 ms +/- 62.1 ms`.
- Candidate was `1.11x +/- 0.11` faster.

Reversed SET/P16/C50/1M:

- Candidate: `678.4 ms +/- 25.2 ms`.
- Baseline: `689.5 ms +/- 54.0 ms`.
- Candidate was `1.02x +/- 0.09` faster.

## Decision

Reject. The paired order looked positive, but the reversed order collapsed to a
small noisy effect and the result is below the bead's `>=1.20x` target. The
production source tree is restored to no-diff state; only this rejection bundle
is retained.

Score: `1.5 < 2.0`.

Next route should move deeper than output-buffer capacity reuse: batch-oriented
parser/execution arenas or a fundamentally different output job primitive.
