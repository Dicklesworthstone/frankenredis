# frankenredis-6kecb pass 3 proof

## Target

- Bead: `frankenredis-6kecb`
- Profile-backed hotspot: SETEX/PSETEX P16/1M, 50 clients, keyspace 10k,
  value size 3.
- Residuals attacked from the pass2 server-only SETEX profile:
  - `2.16%` `fr_protocol::parse_command_args_borrowed_into`
  - `0.97%` `copy_borrowed_argv_into_scratch`
- Build discipline:
  - Baseline: `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass3-base-rch cargo build --profile release-perf -p fr-server -p fr-bench`
  - Candidate: `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass3-candidate-rch cargo build --profile release-perf -p fr-server -p fr-bench`
  - Same rch worker for check/build: `vmi1227854`

## Baseline

Baseline current-main standalone hyperfine:

- SETEX mean: `6.75444594478s +/- 0.04260824596s`
- PSETEX mean: `6.71650582758s +/- 0.03222886233s`

## Lever Tested

One production lever was tested and then removed: add a stack-backed parser for
small borrowed multibulk command argv (`<= 8` args), with exact fallback to the
existing heap-backed borrowed parser for larger argv counts.

The SETEX/PSETEX hot path has four argv entries, so this tested whether
replacing the per-frame argv `Vec` allocation with fixed stack storage could
reduce parser overhead without changing the command router, runtime dispatch,
store mutation, expiration side effects, or response emission.

## Behavior Proof

Golden transcript:

- Baseline bytes: `1877`
- Baseline SHA-256:
  `a7bb0d905a88d149f597d9e5986879f62a48ceb38f2d221fb4eb1083bd3340bb`
- Candidate bytes: `1877`
- Candidate SHA-256:
  `a7bb0d905a88d149f597d9e5986879f62a48ceb38f2d221fb4eb1083bd3340bb`
- Equality: `true`

Coverage:

- valid `SETEX` and `PSETEX`
- `PERSIST` proves expiry existed, followed by deterministic `PTTL == -1`
- lower/mixed-case command names
- invalid TTL and wrong-arity fallback
- non-DB0 selection and isolation
- `MULTI`/`EXEC` queued expiration writes
- RESP3 `HELLO` path
- `CLIENT REPLY` suppression covered by focused runtime tests in pass2 and
  untouched by this parser-storage lever

Isomorphism:

- Ordering is unchanged; parser output feeds the same per-client frame loop and
  reply emission path.
- Tie-breaking is unchanged; no sorted iteration or conflict ordering changed.
- Floating point is not used by this command family.
- RNG is untouched by server execution; benchmark key choice is outside
  observable command semantics.
- Expiry semantics are unchanged in the golden transcript.
- Large argv, invalid bulk elements, null arrays, empty arrays, arity errors,
  and parse errors fall back to or match the existing parser behavior.

Focused validation while the candidate was applied:

- `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass3-check-rch cargo check -p fr-protocol -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass3-test-rch cargo test -p fr-protocol parse_small_command_args_borrowed -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-6kecb-pass3-server-test-rch cargo test -p fr-server process_buffered_frames -- --nocapture`

## Benchmarks

Paired SETEX:

- Baseline: `6.72060801942s +/- 0.01529969428s`
- Candidate: `6.79634438722s +/- 0.06320501339s`
- Summary: baseline `1.01x +/- 0.01` faster

Paired PSETEX:

- Baseline: `6.74478517166s +/- 0.06202501377s`
- Candidate: `6.71852770346s +/- 0.03376398383s`
- Summary: candidate `1.00x +/- 0.01` faster

## Decision

Reject under the Score>=2.0 keep gate.

- Impact: 0
- Confidence: 4
- Effort: 2
- Score: 0.0

No production source hunk is retained. SETEX regressed on the paired
same-worker benchmark, while PSETEX was effectively flat.

## Next Route

Stop the local small-argv parser family for `frankenredis-6kecb`. The next pass
should attack a materially deeper primitive: a pipeline packet parser that
parses a contiguous read-buffer batch into command descriptors once, then feeds
dispatch and OK/error response emission from that packet. Target ratio: at least
`1.08x` on SETEX/PSETEX P16/1M by reducing per-command metadata setup and argv
copy traffic across a whole pipeline, while preserving invalid-frame error
precedence, arity/unknown-command precedence, transaction/pubsub/ACL ordering,
propagation bytes, TTL semantics, commandstats/errorstats, and golden SHA-256.
