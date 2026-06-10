# Pass116 Rejection: Lazy Short Integer Text Representation

Bead: `frankenredis-z52ql`
Base: `f1ab12507`

## Target

Pass116 reprofiled the current server before editing:

- SET/P16/C50/1M baseline: `785.4 ms +/- 17.2 ms`.
- Server-only SET/P16/C50/3M: `1628678.9259873782 ops/sec`.
- 5K perf samples, 0 lost.
- Top resolved user-space row: `fr_store::canonical_string_value_from_slice`
  at `4.22%` self.
- Related rows: `execute_plain_set_borrowed` `1.60%` self,
  `Store::set_plain_borrowed` `1.30%` self.

## Lever Tested

Candidate: add `Value::IntegerText(SmallStr)` for unambiguous short canonical
i64 text, so SET can preserve Redis `int` object semantics without immediately
converting hot decimal payloads to `i64`.

This was deliberately different from the rejected pass110 short-positive parser
fast path: the candidate deferred numeric materialization instead of parsing to
an integer faster. The candidate patch is retained for audit at:

- `artifacts/optimization/frankenredis-ohsk5/pass116/candidate/rejected-source-hunk.patch`

The production source hunk was removed after the benchmark gate failed.

## Isomorphism Proof

Baseline and candidate TCP transcripts matched byte-for-byte.

- Golden sha256:
  `ed6eb02107fc024f5a903539daaaac99d2123b8b194fdcbf0cd7cdefa305230a`.
- Request sha256:
  `14a42bc4c9c744ee28ca89f4167c42309f4acff12743e030ab13efa9fc111b36`.
- Covered GET bytes, `OBJECT ENCODING`, `OBJECT REFCOUNT`, `DEBUG DIGEST`,
  `INCRBY`, overflow on `i64::MAX`, underflow on `i64::MIN`, leading-zero
  noncanonical strings, positive values, and negative values.

The no-diff rejection leaves production ordering, reply bytes, object encoding,
DEBUG DIGEST, persistence, expiry, floating-point behavior, RNG behavior, and
keyspace side effects unchanged.

## Validation Run On Candidate

- `cargo check -p fr-store --all-targets` via rch local fallback: passed.
- `cargo test -p fr-store short_integer_text_representation_keeps_canonical_boundaries`
  via rch local fallback: passed.
- `cargo test -p fr-store set_plain_borrowed_matches_set_for_new_integer_and_string_values`
  via rch local fallback: passed.
- `cargo test -p fr-store integer_string_values_keep_string_semantics_without_vec_payload`
  via rch local fallback: passed.
- `cargo build -p fr-server -p fr-bench --profile release-perf` via rch local
  fallback: passed.

## Performance

Paired SET/P16/C50/1M:

- Baseline: `718.9 ms +/- 40.4 ms`.
- Candidate: `752.0 ms +/- 48.5 ms`.
- Baseline was `1.05x +/- 0.09` faster.

Reversed SET/P16/C50/1M:

- Candidate: `705.8 ms +/- 19.6 ms`.
- Baseline: `729.2 ms +/- 42.5 ms`.
- Candidate was only `1.03x +/- 0.07` faster.

## Decision

Reject. The paired and reversed runs are contradictory and both effects are
small; this does not clear the `Score>=2.0` keep gate.

Score: `0.6 < 2.0`.

## Next Route

Do not repeat integer canonicalization representation or parse micro-family
work. The next pass should route deeper to the shifted server cost:

- write-side IO batching / completion wake reduction, because writer `__send`
  remains dominant and worker fanout already failed; or
- an event-loop output primitive that reduces per-client wake/epoll/syscall
  churn without changing reply ordering.

Target ratio for the next alien primitive: `>=1.20x` on SET/P16/C50 with the
same paired and reversed benchmark discipline.
