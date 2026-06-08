# frankenredis-5srqd pass67 rejection proof

## Target

- Bead: `frankenredis-5srqd`
- Profile-backed hotspot: GET P16 with 50 clients / pipeline 16.
- Parent: `2aa9b6320` (`fix(pubsub): arity/unknown check precedes RESP2 subscribe-context gate`)
- Candidate lever tested: maintain recorded-session CLIENT TRACKING counters incrementally and use them in `refresh_store_runtime_info_context` instead of scanning `server.client_sessions` for `tracking_clients` and `tracking_total_prefixes`.

## Profile Evidence

Fresh post-APPEND profile artifact:

- `baseline-get-p16-1m-profile-run.json`
- `baseline-get-p16-1m.perf.data`
- `baseline-get-p16-1m-user-self-report.txt`

Key sampled costs:

- `ClientConnection::try_flush`: 38.96% children.
- `refresh_store_runtime_info_context`: 7.41% self / 13.86% children.
- `Store::drop_if_expired`: 9.19% children, but no-expiry/single-probe GET variants were already rejected.
- `clock_gettime` / vDSO time reads: about 8.55% children.

## Behavior Proof

Golden harness: `run_tracking_golden.py`.

It runs parent and candidate servers, exercises two TCP sessions through CLIENT TRACKING ON/OFF plus `INFO clients` and `INFO stats`, then hashes a normalized transcript containing only the touched observable fields:

- `tracking_clients`
- `tracking_total_prefixes`
- RESP OK replies for tracking state changes

Result:

- Baseline SHA-256: `d4a9346460558d9cf4137cee8d76445677fb9de87b74475f5008600e46c8f17e`
- Candidate SHA-256: `d4a9346460558d9cf4137cee8d76445677fb9de87b74475f5008600e46c8f17e`
- Parity: true

Isomorphism notes:

- Command ordering unchanged: only pre-existing INFO counter derivation was altered in candidate.
- Tie-breaking unchanged: BTreeMap/BTreeSet order was not changed in shipped code.
- Floating-point unchanged: no FP operations involved.
- RNG unchanged: no RNG operations involved.
- Network reply bytes for the normalized tracking transcript were identical parent vs candidate.

Focused candidate-only unit proof before rejection:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-5srqd-test-target cargo test -p fr-runtime info_tracking_session_counts_stay_exact_after_record_replace_and_remove -- --nocapture`
- Result: passed.
- `ubs $(git diff --name-only --cached)` exited 0 before commit; it reported
  artifact-script subprocess warnings but no blocking finding.

## Benchmarks

Builds:

- Parent build: `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-5srqd-parent-target cargo build --profile release-perf -p fr-server -p fr-bench`
- Candidate build: `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-5srqd-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`

GET P16 / 300k, 50 clients:

- Parent: `497.8 ms +/- 16.2 ms`
- Candidate: `502.1 ms +/- 16.7 ms`
- Result: candidate slower/tied.

Paired GET P16 / 1M, 50 clients:

- Parent: `1.411 s +/- 0.023 s`
- Candidate: `1.390 s +/- 0.029 s`
- Ratio: candidate `1.01x +/- 0.03`

## Decision

Rejected. Score is below the keep threshold:

- Impact: 1
- Confidence: 2
- Effort: 2
- Score: 1.0

No production code from this lever is kept.

## Next Primitive

The next pass should stop pursuing metadata-refresh micro variants and attack the larger profile-backed output/syscall family:

- Candidate primitive: output-buffer cursor/ring buffer or writev slab batching for `ClientConnection::try_flush`.
- Target: reduce drain/memmove and write syscall path cost on GET P16.
- Source inspiration: alien-graveyard syscall batching / registered-buffer / batched I/O primitives.
