# frankenredis-6kecb pass 2 proof

## Target

- Bead: `frankenredis-6kecb`
- Profile-backed hotspot: SETEX/PSETEX P16/1M, 50 clients, keyspace 10k,
  value size 3.
- Build discipline:
  - Baseline: `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass2-base-rch cargo build --profile release-perf -p fr-server -p fr-bench`
  - Candidate: `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass2-candidate-rch cargo build --profile release-perf -p fr-server -p fr-bench`

## Baseline and Profile

Baseline current-main standalone hyperfine:

- SETEX mean: `6.72836177834s +/- 0.06516675391s`
- PSETEX mean: `6.67213293774s +/- 0.07855468899s`

Server-only SETEX profile top residuals:

- `5.31%` SipHash `Hasher::write`
- `4.05%` `Runtime::refresh_store_runtime_info_context`
- `3.00%` `Runtime::execute_frame_internal`
- `2.16%` `fr_protocol::parse_command_args_borrowed_into`
- `1.62%` `Runtime::execute_dispatch`
- `1.58%` `Store::internal_entries_insert`
- `1.55%` `Runtime::dispatch_with_client_context`
- `1.54%` `mi_free`
- `0.97%` `copy_borrowed_argv_into_scratch`

## Lever Tested

One production lever was tested and then removed: avoid the full
`sync_dispatch_client_context_to_session` clone-back for generic commands that
cannot mutate session-visible client context. The candidate preserved
`CLIENT REPLY SKIP` by copying only the dispatch reply-suppression bits back on
ordinary commands, while retaining the full sync for `CLIENT`/`SELECT`.

This attacked the profiled `dispatch_with_client_context` and
`ClientTrackingState::clone_from` residual, not the rejected SETEX/PSETEX
command-specific borrowed branch.

## Behavior Proof

Golden transcript:

- Baseline bytes: `1372`
- Baseline SHA-256:
  `369a4f022d386770e2613e902910b21a48d9476428461c1a026da369b9b99e13`
- Candidate bytes: `1372`
- Candidate SHA-256:
  `369a4f022d386770e2613e902910b21a48d9476428461c1a026da369b9b99e13`
- Equality: `true`

Coverage:

- valid `SETEX` and `PSETEX`
- `PERSIST` proves expiry existed, followed by deterministic `PTTL == -1`
- lower/mixed-case command names
- invalid TTL and wrong-arity fallback
- non-DB0 selection and isolation
- `MULTI`/`EXEC` queued expiration writes
- RESP3 `HELLO` path
- `CLIENT REPLY` suppression covered by focused runtime tests

Isomorphism:

- Ordering is unchanged; dispatch and reply emission remain in per-client frame
  order.
- Tie-breaking is unchanged; no sorted iteration or conflict ordering changed.
- Floating point is not used by this command family.
- RNG is untouched.
- Expiry semantics are unchanged in the golden transcript.
- `CLIENT REPLY SKIP` one-shot suppression was preserved by copying the exact
  dispatch suppression bits back to the session for ordinary commands.

Focused validation while the candidate was applied:

- `cargo fmt --check` was attempted and failed on broad pre-existing formatting
  drift outside this pass; no workspace format pass was applied.
- `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass2-check-rch cargo test -p fr-runtime client_reply_state_transitions_match_redis -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass2-check-rch cargo test -p fr-runtime client_reply -- --nocapture`

## Benchmarks

Paired SETEX:

- Baseline: `6.90293452756s +/- 0.07178622301s`
- Candidate: `6.95625542396s +/- 0.08535175597s`
- Summary: baseline `1.01x +/- 0.02` faster

Paired PSETEX:

- Baseline: `6.88715643772s +/- 0.06499850177s`
- Candidate: `6.98469510632s +/- 0.08000598493s`
- Summary: baseline `1.01x +/- 0.02` faster

## Decision

Reject under the Score>=2.0 keep gate.

- Impact: 0
- Confidence: 4
- Effort: 2
- Score: 0.0

No production source hunk is retained. The measured effect is a small regression
on both targeted modes.

## Next Route

Stop this context-sync micro-lever family for `frankenredis-6kecb`. The next
pass should attack the bead's deeper primitive directly: a batched
parser-to-dispatch packet and/or output arena that amortizes argv scratch copies,
command metadata, and small OK reply writes across a pipeline while preserving
invalid frame errors, arity/unknown precedence, ACL/pubsub/transaction ordering,
propagation bytes, TTL semantics, commandstats/errorstats, and golden SHA-256.
