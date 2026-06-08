# frankenredis-6kecb pass 1 proof

## Target

- Bead: `frankenredis-6kecb`
- Profile-backed hotspot: SETEX/PSETEX P16/1M, 50 clients, keyspace 10k,
  value size 3.
- Baseline build:
  `rch exec -- env CARGO_TARGET_DIR=target-6kecb-base-rch cargo build --profile release-perf -p fr-server -p fr-bench`
- Candidate build:
  `rch exec -- env CARGO_TARGET_DIR=target-6kecb-candidate-rch cargo build --profile release-perf -p fr-server -p fr-bench`

## Baseline

- SETEX mean: `6.76094814798s +/- 0.0544498261s`
- PSETEX mean: `6.82346704578s +/- 0.1075686851s`
- Server-only SETEX profile top residuals included SipHash writes,
  `Runtime::refresh_store_runtime_info_context`, `Runtime::execute_frame_internal`,
  `process_buffered_frames`, `parse_command_args_borrowed_into`,
  `dispatch_with_client_context`, `__memmove_avx_unaligned_erms`,
  `Store::update_expiry_deadline`, `mi_free`, and `Store::internal_entries_insert`.

## Lever Tested

One production lever was tested and then removed: consolidate string write
bookkeeping in `fr-store` so `Store::set` and `Store::set_with_abs_expiry`
snapshot the old entry once and avoid the generic `internal_entries_insert`
path's repeated lookup/bookkeeping shape.

This was a class-level store write insertion primitive, not another
command-specific SETEX/PSETEX borrowed branch.

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

Isomorphism:

- Ordering is unchanged; command processing and reply emission remain in
  per-client frame order.
- Tie-breaking is unchanged; no reply-visible sorted iteration or conflict
  ordering changed.
- Floating point is not used by this command family.
- RNG is untouched; LFU/random-key state was preserved in the tested candidate.
- Expiry semantics are unchanged in the golden transcript and focused tests.

Focused validation while the candidate was applied:

- `cargo fmt -p fr-store --check`
- `rch exec -- env CARGO_TARGET_DIR=target-6kecb-candidate-check-rch cargo check -p fr-store -p fr-command -p fr-runtime -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=target-6kecb-candidate-test-rch cargo test -p fr-store set -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-6kecb-candidate-frcommand-test-rch cargo test -p fr-command setex -- --nocapture`

## Benchmarks

Paired SETEX:

- Baseline: `6.61883900352s +/- 0.03667118858s`
- Candidate: `6.65780338592s +/- 0.03738158023s`
- Summary: baseline `1.01x +/- 0.01` faster

Paired PSETEX:

- Baseline: `6.68627370828s +/- 0.01743231297s`
- Candidate: `6.67152508588s +/- 0.12137374963s`
- Summary: candidate `1.00x +/- 0.02` faster

Reversed SETEX:

- Candidate: `6.57912884408s +/- 0.04861704750s`
- Baseline: `6.71204916608s +/- 0.06061749794s`
- Summary: candidate `1.02x +/- 0.01` faster

Reversed PSETEX:

- Candidate: `6.60520201710s +/- 0.05450957049s`
- Baseline: `6.63529104190s +/- 0.01553564851s`
- Summary: candidate `1.00x +/- 0.01` faster

## Decision

Reject under the Score>=2.0 keep gate.

- Impact: 0
- Confidence: 4
- Effort: 2
- Score: 0.0

No production source hunk is retained. The result is order-sensitive/noise-level
and does not approach the required >=1.20x target.

## Next Route

Do not continue store-write bookkeeping micro-levers for this bead. The next
pass should attack the bead's stated algorithmically different primitive:
batched parser-to-dispatch/output packets or a small-reply output arena that
amortizes argv scratch copies, command metadata, and `+OK\r\n` writes across a
pipeline while preserving all fallback states and golden behavior.
