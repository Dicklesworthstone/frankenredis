# frankenredis-zhphm.3 pass4 rejection

Target: post-pass2 SET/P16/1M profile showed `drain_pending_pubsub_to_connection`
on the plain fast path. The trial added a read-only current-client pub/sub
pending predicate and skipped the drain helper when both the legacy store queue
and current-client outbox were empty.

Decision: rejected. Production source hunks were removed before commit.

## Baseline

- Build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p4_baseline cargo build --profile release-perf -p fr-server -p fr-bench`
- Worker: `vmi1227854`
- Benchmark: SET/P16/1M
- Hyperfine mean: `1.2995502886250003s`
- Hyperfine stddev: `0.0767084928869276s`
- Artifact: `baseline/baseline-set-p16-1m-hyperfine.json`

## Candidate

- Build: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p4_candidate cargo build --profile release-perf -p fr-server -p fr-bench`
- Worker: `vmi1227854`
- Benchmark: SET/P16/1M
- Hyperfine mean: `1.301s`
- Hyperfine stddev: `0.048s`
- Artifact: `candidate/candidate-set-p16-1m-hyperfine.json`

## Paired Timing

- Baseline mean: `1.240s +/- 0.035s`
- Candidate mean: `1.198s +/- 0.041s`
- Hyperfine summary: candidate `1.03x +/- 0.05` faster
- Artifact: `paired/paired-set-p16-1m-hyperfine.json`

## Reversed Timing

- Candidate mean: `1.221s +/- 0.033s`
- Baseline mean: `1.215s +/- 0.044s`
- Hyperfine summary: baseline `1.00x +/- 0.05` faster
- Artifact: `reversed/reversed-set-p16-1m-hyperfine.json`

## Behavior Proof

- Transcript SHA-256: `dffd93d857ab070c733ec46d61274b935b4f85fd05992abbf4f979e6959fdf67`
- Baseline response SHA-256: `bdcab17275344bc6c13913945fd5cad11c800de1108c6f77776e16b1a09cc1f4`
- Candidate response SHA-256: `bdcab17275344bc6c13913945fd5cad11c800de1108c6f77776e16b1a09cc1f4`
- Ordering: non-empty pub/sub delivery used the existing drain path, preserving store-queue first, then current-client outbox.
- Tie-breaking, floating point, RNG: not touched.

## Validation

- `cargo fmt -p fr-runtime -p fr-server --check`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p4_test cargo test -p fr-runtime pending_pubsub_predicate_tracks_current_client_queues -- --nocapture`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p4_check cargo check -p fr-server --all-targets`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p4_clippy cargo clippy -p fr-server --all-targets -- -D warnings`: blocked by pre-existing `fr-store` doc-lazy-continuation lint at `crates/fr-store/src/lib.rs:1705`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p4_clippy_nodeps cargo clippy -p fr-server --all-targets --no-deps -- -D warnings`: passed
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_zhphm_p4_conformance cargo test -p fr-conformance --lib -- --nocapture`: passed, `194 passed`; live-oracle checks skipped because vendored Redis server was unavailable in the scratch path

## Score

- Impact: `0.2`
- Confidence: `0.7`
- Effort: `1.0`
- Score: `0.14`, below the `2.0` keep threshold.

## Next Target

Do not repeat pub/sub empty-drain or client-accounting micro-levers. The next
profile-backed pass should attack the parent `frankenredis-zhphm` structural
primitive: safe-Rust IO-thread style offload of read/parse and encode/write,
while keeping command execution serial and behavior-ordered.
