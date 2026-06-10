# Pass118 Rejection: SET Borrowed One-Probe Persistent Overwrite

Bead: `frankenredis-ohsk5.14`
Base: `c51f2d309`

## Target

Pass118 used the fresh pass117 server-only profile:

- `Store::drop_if_expired`: `3.27%` children, `0.26%` self.
- `canonical_string_value_from_slice`: `4.58%` self.
- `Runtime::execute_plain_set_borrowed`: `4.83%` children, `1.07%` self.
- `Timespec::now` / `clock_gettime`: `2.95%` / `2.81%`.

The pass118 baseline SET/P16/C50/1M was `860.4 ms +/- 12.1 ms`.

## Lever Tested

Candidate: change `Store::set_plain_borrowed` so the common existing persistent
key path uses one expiry-aware mutable entry probe instead of calling
`drop_if_expired` and then doing a second mutable lookup. Live volatile and
expired keys kept the slower expiry path. The candidate also skipped
volatile-index/deadline bookkeeping when the old entry had no TTL.

This deliberately avoided the rejected wake-coalescing, tiny sync flush,
integer parser/representation, static-OK, and nonnumeric bypass families. The
rejected source hunk is retained at:

- `artifacts/optimization/frankenredis-ohsk5/pass118/candidate/rejected-source-hunk.patch`

The production source hunk was removed after the benchmark gate failed.

## Isomorphism Proof

Baseline and candidate TCP transcripts matched byte-for-byte.

- Golden sha256:
  `ed6eb02107fc024f5a903539daaaac99d2123b8b194fdcbf0cd7cdefa305230a`.
- Request sha256:
  `14a42bc4c9c744ee28ca89f4167c42309f4acff12743e030ab13efa9fc111b36`.

Focused store tests passed while the candidate was applied:

- `set_plain_borrowed_matches_set_for_existing_volatile_lfu_string`
- `set_plain_borrowed_matches_set_for_existing_persistent_values`
- `set_plain_borrowed_matches_set_for_new_integer_and_string_values`

The no-diff rejection leaves production reply bytes, key existence/order,
expiry deletion ordering, TTL clearing, LFU/LRU updates, stream metadata
cleanup, dirty/modification counters, object encoding, DEBUG DIGEST,
floating-point behavior, RNG behavior, and keyspace side effects unchanged.

## Validation Run On Candidate

- `cargo check -p fr-store --all-targets` via rch local fallback: passed, with
  pre-existing warnings in unrelated in-file perf tests.
- `cargo test -p fr-store set_plain_borrowed -- --nocapture` via rch local
  fallback: passed, with the same pre-existing unrelated warnings.
- `cargo build -p fr-server -p fr-bench --profile release-perf` via rch local
  fallback: passed.
- `cargo fmt --check -p fr-store` reports broad pre-existing rustfmt drift in
  `fr-store/src/lib.rs`; the candidate-owned formatting was normalized manually.

## Performance

Paired SET/P16/C50/1M:

- Baseline: `692.3 ms +/- 20.4 ms`.
- Candidate: `686.4 ms +/- 52.4 ms`.
- Candidate was only `1.01x +/- 0.08` faster.

Reversed SET/P16/C50/1M:

- Candidate: `660.9 ms +/- 43.9 ms`.
- Baseline: `670.9 ms +/- 18.4 ms`.
- Candidate was only `1.02x +/- 0.07` faster.

## Decision

Reject. The effect is directionally positive but too small and noisy for the
`Score>=2.0` keep gate.

Score: `1.0 < 2.0`.

## Next Route

Do not repeat one-probe persistent SET overwrite or `drop_if_expired` micro
families. The next pass needs a structurally different primitive: reprofile
current main and attack either output syscall/job granularity with a bigger
ratio target, or a broader command-batch/arena execution path that removes an
entire class of per-command metadata work.

Target ratio for the next alien primitive: `>=1.20x` on SET/P16/C50 with the
same paired and reversed benchmark discipline.
