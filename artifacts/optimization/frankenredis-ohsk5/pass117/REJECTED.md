# Pass117 Rejection: Writer Completion Wake Coalescing

Bead: `frankenredis-ohsk5.13`
Base: `0844c9997`

## Target

Pass117 reprofiled current main before editing:

- SET/P16/C50/1M baseline: `641.0 ms +/- 36.2 ms`.
- Server-only SET/P16/C50/3M: `1706766.49597001 ops/sec`.
- Profile samples: 0 lost.
- Dominant rows remained writer-thread `__send` (`23.24%` and `22.56%`
  self in the two writer threads).
- Profile-backed wake surface: `mio::Waker::wake`/`__GI___libc_write` around
  `4.45%` combined, and `drain_writer_completions` at `0.99%` children.

## Lever Tested

Candidate: coalesce writer completion wakes with a shared atomic flag in
`WriterPool`. Workers sent each `WriterCompletion` to the channel, but only
called `Waker::wake` on the first queued completion. The event loop disarmed
the flag after draining and performed a final `try_recv` to close the race where
a completion arrives between an empty receive and disarm.

This preserved the existing one-in-flight job per client and did not change the
completion channel ordering. The rejected production hunk is retained at:

- `artifacts/optimization/frankenredis-ohsk5/pass117/candidate/rejected-source-hunk.patch`

The production source hunk was removed after the benchmark gate failed.

## Isomorphism Proof

Baseline and candidate TCP transcripts matched byte-for-byte.

- Golden sha256:
  `ed6eb02107fc024f5a903539daaaac99d2123b8b194fdcbf0cd7cdefa305230a`.
- Request sha256:
  `14a42bc4c9c744ee28ca89f4167c42309f4acff12743e030ab13efa9fc111b36`.
- Covered SET/GET, OBJECT ENCODING/REFCOUNT, DEBUG DIGEST, INCR/DECR overflow
  boundaries, CLIENT-visible reply bytes, and QUIT connection close.

The no-diff rejection leaves production reply ordering, output buffering,
backpressure, expiry/order, floating-point behavior, RNG behavior, keyspace side
effects, and replication ordering unchanged.

## Validation Run On Candidate

- `cargo fmt --check -p fr-server`: passed.
- `cargo check -p fr-server --all-targets` via rch local fallback: passed.
- `cargo test -p fr-server --all-targets` via rch local fallback: passed.
- `cargo build -p fr-server -p fr-bench --profile release-perf` via rch local
  fallback: passed.

## Performance

Paired SET/P16/C50/1M:

- Baseline: `725.1 ms +/- 28.5 ms`.
- Candidate: `755.0 ms +/- 31.8 ms`.
- Baseline was `1.04x +/- 0.06` faster.

Reversed SET/P16/C50/1M:

- Candidate: `687.6 ms +/- 49.2 ms`.
- Baseline: `708.8 ms +/- 42.5 ms`.
- Candidate was only `1.03x +/- 0.10` faster.

## Decision

Reject. The paired run regressed and the reversed run only showed a tiny,
high-noise apparent win. This does not clear the `Score>=2.0` keep gate.

Score: `0.7 < 2.0`.

## Next Route

Do not repeat writer wake-flag coalescing. The profile says the larger bottleneck
is still per-reply writer `__send`, while main-thread expiry/time and value
canonicalization are smaller but resolved. The next pass should attack a
different primitive, such as safe-Rust output syscall batching that changes job
granularity without changing per-client reply order, or a fresh profile-backed
expiry/time fast path if the post-rejection profile keeps `drop_if_expired` and
`clock_gettime` hot.

Target ratio for the next alien primitive: `>=1.20x` on SET/P16/C50 with the
same paired and reversed benchmark discipline.
