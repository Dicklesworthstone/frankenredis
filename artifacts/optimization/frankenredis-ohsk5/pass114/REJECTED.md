# Pass114 Rejection: Tiny Reply Inline Flush

Bead: `frankenredis-ohsk5.12`
Commit base: `a9f1aacd0`

## Target

Pass113 and pass114 profiles kept `__send` hot in the writer path after the
direct borrowed SET parser landed. The fresh pass114 server/client profile for
SET/P16/C50/3M reached `1391311.2487298807 ops/sec`, p50 `536us`, p95 `825us`,
p99 `1117us`, with 28749 perf samples and 0 lost samples. Relevant rows:

- `fr-writer-1` `__send`: `9.76%` self.
- `fr-writer-0` `__send`: `9.21%` self.
- `fr-bench` `__send`: `18.20%` self / `18.14%` child path.

The client-side `fr-bench` sender samples mean this is not yet a clean
server-only profile, but the writer-side rows were large enough to test one
bounded output lever.

## Lever Tested

Candidate: only hand off output buffers of at least `1024` bytes to the writer
pool. Tiny SET reply batches were flushed synchronously on the existing
nonblocking main-thread path, while larger buffers kept the current writer-pool
behavior.

The candidate source hunk was removed after benchmarking.

## Isomorphism Proof

Baseline and candidate raw TCP RESP transcripts matched exactly.

- Golden sha256: `27fde3960948e19fe73956e617c34b409981279579471ef94feb6af1ebe6e30e`.
- Covered mixed-case direct SET, GET, SET NX fallback, pipelined SET replies,
  CLIENT REPLY OFF suppression, CLIENT REPLY ON, PING, and GET after suppressed
  write.
- Reply lengths matched: `[5, 9, 5, 5, 9, 10, 0, 0, 5, 7, 9]`.

The no-diff rejection preserves reply bytes, per-client ordering, expiry,
key policy, floating-point behavior, RNG behavior, and object/digest semantics.

## Performance

Baseline before edit:

- SET/P16/C50/1M hyperfine: `841.7 ms +/- 73.4 ms`.

Paired SET/P16/C50/1M:

- Baseline: `795.0 ms +/- 72.0 ms`.
- Candidate: `1.279 s +/- 0.041 s`.
- Baseline was `1.61x +/- 0.15` faster.

Reversed SET/P16/C50/1M:

- Candidate: `1.305 s +/- 0.046 s`.
- Baseline: `821.4 ms +/- 91.4 ms`.
- Baseline was `1.59x +/- 0.19` faster.

## Decision

Reject. The writer pool is buying enough event-loop decoupling that inline tiny
flushes regress the SET/P16 workload despite avoiding the cross-thread handoff.

Score: `0.0 < 2.0`.

## Next Route

Do not repeat tiny synchronous flushing. The next pass should collect a
server-only profile so `fr-bench` sender `__send` samples do not distort the
target ranking, then attack the top server row with one structurally different
primitive.
