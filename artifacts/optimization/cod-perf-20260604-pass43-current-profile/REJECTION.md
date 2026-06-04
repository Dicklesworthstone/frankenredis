# Pass 43 Rejection: Slowlog Metadata Pre-Gate

Bead: `frankenredis-yaxr7.1`

Profile target:
- `baseline-perf-report-nochildren.txt` showed formatting-related self time in the
  SET pipeline=16 hot path alongside `Runtime::record_slowlog`.
- Candidate lever checked `slowlog_log_slower_than_us` before peer address
  formatting and client-name cloning in `Runtime::record_slowlog`.

Behavior proof:
- The candidate preserved ordering, tie-breaking, floating-point, and RNG behavior:
  it only skipped metadata materialization before the existing slowlog threshold
  gate for commands below threshold.
- Focused test passed before rejection:
  `RCH_FORCE_REMOTE=true rch exec -- env CARGO_TARGET_DIR=target-cod-pass43-slowlog-test-rch cargo test -p fr-runtime slowlog_below_threshold_keeps_log_and_id_unchanged -- --nocapture`
- Committed state keeps no runtime source change from this lever, so behavior
  parity is identity for the landed commit.

Validation:
- `RCH_FORCE_REMOTE=true rch exec -- env CARGO_TARGET_DIR=target-cod-pass43-slowlog-check-rch cargo check -p fr-runtime --all-targets`
- `RCH_FORCE_REMOTE=true rch exec -- env CARGO_TARGET_DIR=target-cod-pass43-slowlog-clippy-rch cargo clippy -p fr-runtime --all-targets -- -D warnings`
- `cargo fmt -p fr-runtime --check`
- `ubs crates/fr-runtime/src/lib.rs` reported pre-existing file-wide findings,
  with no finding introduced on the touched slowlog hunk.

Benchmarks:
- Initial saved baseline: `2.565s +/- 0.253s`
- Candidate: `1.749s +/- 0.173s`
- Paired baseline confirmation: `1.724s +/- 0.081s`
- Paired result: candidate was 1.4% slower than confirmation baseline, inside
  noise and below the Score >= 2.0 keep gate.

Artifact SHA256:
- `6226cdc627c0f07f8b801307ee3a269926901ac748bf710771c5ac8fe08ce52a  baseline-set-p16-hyperfine.json`
- `3518e96ce80eb9bb1cc446cde5eaeade2e08eca0fa6925e868bbe9e2c1676320  baseline-confirm-set-p16-hyperfine.json`
- `1be7bab0e98fdf9f8c5eaf3c80c41a2e2e75a48ac6b993ceb31c1c0eea3b79f5  candidate-set-p16-hyperfine.json`
- `0c13f0b84d685df77dce0ddf75e017f9d8d65a2f145ab4b546aec1352d69a6da  baseline-perf-report-nochildren.txt`
- `8e0ded772686c33818137791c7b1c16b131be8361e0ffa84023c7038c71449d0  baseline-strace-summary.txt`

Next primitive:
- Pivot to the larger profile-backed bcast client-tracking registry in
  `frankenredis-yaxr7`, replacing the all-client invalidation scan for write
  commands when no clients are tracking.
