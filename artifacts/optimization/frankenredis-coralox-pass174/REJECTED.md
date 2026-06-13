# frankenredis-ohsk5.46 rejected: true HSET batch parser/executor primitive

## Target

- Bead: `frankenredis-ohsk5.46`
- Profile-backed hotspot: HSET P16/C50 remained the active residual after the pass-172 sweep and the pass-173 packet shim rejection:
  - pass-172 current-main sweep: FrankenRedis `599439.47 ops/sec`, Redis `885435.73 ops/sec`, Redis/fr `1.477x`
  - later standard-row evidence still showed a residual around `1.114x`
- Candidate lever: parse consecutive canonical `HSET key field value` RESP packets from one readable buffer into a batch path, execute each through the existing borrowed HSET runtime executor, drain pub/sub after each command, and check the output limit after each command.

## Baseline

- Built baseline with `rch`:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass174-baseline-target`
  - command: `cargo build --profile release-perf -p fr-server -p fr-bench`
- Independent baseline HSET P16/C50/n1M:
  - mean: `1.437018538s`
  - stddev: `0.036167037s`
  - last-run throughput: `732083.95 ops/sec`

## Behavior Proof

- Focused parser tests passed with `rch`:
  - `cargo test -p fr-server borrowed_plain_hset_packet_parser -- --nocapture`
- Golden raw TCP transcript:
  - input SHA256: `6545d13252cbdce84b56f2986b3c49676407e78e74278ce0b24ee70122f2ef6a`
  - baseline output SHA256: `123ada06de8f0eeb6f22c93b29fd6d8256a89a8a14abd5e6f85342de140645d7`
  - candidate output SHA256: `123ada06de8f0eeb6f22c93b29fd6d8256a89a8a14abd5e6f85342de140645d7`
- Isomorphism:
  - Ordering/tie-breaking: the batch loop executed one HSET at a time in input order and appended each reply before moving to the next packet.
  - Pub/sub/output ordering: pending pub/sub messages were drained immediately after each HSET, matching the generic loop.
  - Error precedence: noncanonical and multi-field HSET packets fell back to the generic parser/dispatcher.
  - Output-limit behavior: the candidate checked the output hard limit after each command and stopped the batch at the same disconnect boundary.
  - Floating point: no FP paths touched.
  - RNG: no RNG/LFU sampling path touched.

## Re-benchmark

- Paired HSET P16/C50/n1M hyperfine:
  - baseline: `1.426916927s +/- 0.169159429s`
  - candidate: `1.256672151s +/- 0.041618494s`
  - summary: candidate `1.14 +/- 0.14x` faster
- Confirmation HSET P16/C50/n2M hyperfine:
  - baseline: `2.627455885s +/- 0.075156410s`
  - candidate: `2.574508297s +/- 0.238529159s`
  - summary: candidate `1.02 +/- 0.10x` faster
- Score:
  - `0.1 impact * 0.35 confidence / 1.5 effort = 0.023`
  - Fails the required `Score>=2.0` keep gate.

## Decision

- Rejected.
- Production source hunk and candidate-only tests were removed before commit.
- Evidence retained in this directory.

## Next Route

Stop HSET single-command and same-executor batch parser shims. Attack a different primitive next: parser arena/region reuse for borrowed argv and reply construction across an entire readable batch, or a store-layout/key-comparison primitive that reduces repeated hash/key probes for HSET without adding per-command branch work.
