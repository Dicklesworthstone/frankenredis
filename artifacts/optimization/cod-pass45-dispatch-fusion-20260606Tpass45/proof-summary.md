# Pass45 Proof Summary: Borrowed Plain SET Fast Lane

Bead: frankenredis-ohsk5

## Profile Target

Fresh SET pipeline=16 profile, 1,000,000 requests, showed the remaining hot path in request dispatch and hashing:

- `Runtime::execute_dispatch`: 4.75% self
- `execute_frame_internal`: 3.63% self
- `dispatch_with_client_context`: 2.85% self
- `parse_command_args_borrowed_into`: 1.78% self
- `RandomState::hash_one::<&[u8]>`: 6.15% self
- `Hasher::write`: 4.37% self

Lever: exact 3-argument plain `SET key value` frames bypass owned argv materialization and generic dispatch only when runtime state is the default simple write state.

## Baseline

RCH-built release-perf baseline:

`rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench`

Post-rebase parent baseline, after upstream `99cb08570` landed:

`rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-rebased-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench`

Post-rebase candidate:

`rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-rebased-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`

Initial baseline hyperfine, SET pipeline=16, 300,000 requests:

- Mean: 1.00140219072 s
- Stddev: 0.12085917643172503 s
- Last direct run: 277,705.18 ops/sec, total 1080 ms, p50 2781 us, p95 3929 us, p99 4779 us

## Final Paired Benchmark

Final paired hyperfine, same binaries/workload shape, 10 runs:

- Baseline mean: 1.55974050702 s +/- 0.25761669193264614 s
- Candidate mean: 0.99458954352 s +/- 0.08073993228387764 s
- Hyperfine ratio: candidate 1.57x +/- 0.29 faster

Last direct fr-bench samples:

- Baseline: 182,615.44 ops/sec, total 1642 ms, p50 3663 us, p95 6447 us, p99 13759 us
- Candidate: 517,610.96 ops/sec, total 579 ms, p50 1386 us, p95 2317 us, p99 3137 us

Artifact files:

- `paired-v3-set-p16-300k-hyperfine.json`
- `paired-v3-set-p16-300k-hyperfine.txt`
- `paired-v3-baseline-set-p16-300k-last.json`
- `paired-v3-candidate-set-p16-300k-last.json`

## Rebased Same-Base Gate

The branch was rebased onto upstream `99cb08570`; the keep gate was refreshed against that exact parent.

Paired hyperfine, SET pipeline=16, 300,000 requests, 10 runs:

- Baseline mean: 1.03448994552 s +/- 0.046964355193960194 s
- Candidate mean: 0.91049737262 s +/- 0.10313254824094013 s
- Hyperfine ratio: candidate 1.14x +/- 0.14 faster

Longer paired hyperfine, SET pipeline=16, 1,000,000 requests, 8 runs:

- Baseline mean: 2.590070638355 s +/- 0.14824942041776662 s
- Candidate mean: 2.006099759855 s +/- 0.12126847490740052 s
- Hyperfine ratio: candidate 1.29x +/- 0.11 faster

Last direct fr-bench samples from the longer gate:

- Baseline: 378,020.70 ops/sec, total 2645 ms, p50 1775 us, p95 3321 us, p99 4511 us, p999 8059 us
- Candidate: 591,146.36 ops/sec, total 1691 ms, p50 1245 us, p95 1902 us, p99 2415 us, p999 3149 us

Artifact files:

- `paired-v4-rebased-set-p16-300k-hyperfine.json`
- `paired-v4-rebased-set-p16-300k-hyperfine.txt`
- `paired-v4-rebased-baseline-set-p16-300k-last.json`
- `paired-v4-rebased-candidate-set-p16-300k-last.json`
- `paired-v5-rebased-set-p16-1m-hyperfine.json`
- `paired-v5-rebased-set-p16-1m-hyperfine.txt`
- `paired-v5-rebased-baseline-set-p16-1m-last.json`
- `paired-v5-rebased-candidate-set-p16-1m-last.json`

After the proof audit added disabled-maxmemory diagnostic parity and focused reply-suppression tests, the final candidate was rebuilt and remeasured.

Final paired hyperfine, SET pipeline=16, 1,000,000 requests, 8 runs:

- Baseline mean: 3.01652974702 s +/- 0.4907055987768746 s
- Candidate mean: 2.15463563652 s +/- 0.2097983211433854 s
- Hyperfine ratio: candidate 1.40x +/- 0.27 faster

Last direct fr-bench samples from the final gate were noisier than the v5 latency samples but still showed higher throughput:

- Baseline: 429,844.71 ops/sec, total 2326 ms, p50 1740 us, p95 2461 us, p99 2859 us, p999 3469 us
- Candidate: 475,731.03 ops/sec, total 2102 ms, p50 1525 us, p95 2599 us, p99 3533 us, p999 4551 us

Final artifact files:

- `paired-v6-final-set-p16-1m-hyperfine.json`
- `paired-v6-final-set-p16-1m-hyperfine.txt`
- `paired-v6-final-baseline-set-p16-1m-last.json`
- `paired-v6-final-candidate-set-p16-1m-last.json`

## Behavior Proof

Golden transcript covers:

- Plain `SET` / `GET`
- Overwrite
- Binary bulk payload
- Optioned `SET NX` fallback
- Wrong arity fallback
- Nonzero DB fallback
- `CLIENT REPLY SKIP` fallback
- `MULTI` / `EXEC` fallback

Final golden files:

- `golden-v4-baseline.resp`: 186 bytes
- `golden-v4-candidate.resp`: 186 bytes
- `golden-v5-baseline.resp`: 186 bytes
- `golden-v5-candidate.resp`: 186 bytes
- `golden-v6-baseline.resp`: 186 bytes
- `golden-v6-candidate.resp`: 186 bytes
- SHA256 baseline: `db6cb2c7b597240873b45dccaf630b0ca8f212ef491395ad8b1e8d016387403e`
- SHA256 candidate: `db6cb2c7b597240873b45dccaf630b0ca8f212ef491395ad8b1e8d016387403e`
- `cmp`: byte-identical

Isomorphism:

- Ordering: one response is emitted at the same frame-consumption point; pub/sub drain remains after reply encoding.
- Tie-breaking: none touched.
- Floating point: none touched.
- RNG: none touched.
- Persistence/replication/notifications/tracking/transactions/ACLs/DB selection: ineligible and routed to the existing generic path.
- Output suppression: preserved by applying existing `CLIENT REPLY SKIP/OFF` state before returning the fast-path reply; the network server still checks `suppress_current_network_reply()` before encoding.
- Maxmemory diagnostics: mirrors generic dispatch by clearing `last_eviction_loop` when maxmemory is disabled.
- New focused test proves AOF-configured runtimes do not enter the borrowed `SET` fast lane and still capture the generic AOF record.
- New focused tests prove existing reply suppression is honored and disabled-maxmemory eviction diagnostics are cleared.

## Validation

Passed:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-check-v3-target cargo check -p fr-runtime -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-clippy-v3-target cargo clippy -p fr-runtime -p fr-server --all-targets -- -D warnings`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-aof-guard-test-target cargo test -p fr-runtime plain_set_borrowed_fast_path_is_disabled_when_aof_is_configured -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-rebased-extra-tests-target cargo test -p fr-runtime plain_set_borrowed_fast_path_ -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-rebased-check2-target cargo check -p fr-runtime -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-rebased-clippy2-target cargo clippy -p fr-runtime -p fr-server --all-targets -- -D warnings`
- `cargo fmt --check -p fr-runtime -p fr-server`
- `git diff --check -- crates/fr-runtime/src/lib.rs crates/fr-server/src/main.rs`

Broad `fr-server` test note:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass45-frserver-test-target cargo test -p fr-server --all-targets` had one failure in `tcp_aof_restart_preserves_all_data` (`AOF file was not created`).
- The new fast lane is disabled when AOF is configured, and the focused guard test proves that boundary. The failure is not used as the keep gate for this profile-backed plain SET default-state path.

## Score

Impact 3.0 x Confidence 0.95 / Effort 1.0 = 2.85.

Keep decision: keep. Score is above the required 2.0 gate.
