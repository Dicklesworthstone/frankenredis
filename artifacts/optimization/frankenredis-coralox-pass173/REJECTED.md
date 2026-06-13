# frankenredis-ohsk5.44 rejected: canonical HSET packet parser/executor shim

## Target

- Bead: `frankenredis-ohsk5.44`
- Profile-backed hotspot: HSET P16/C50 remained the largest current-main gap in the pass-172 sweep:
  - FrankenRedis: `599439.47 ops/sec`
  - Redis: `885435.73 ops/sec`
  - Redis/fr ratio: `1.477x`
- This pass tested one lever only: a canonical `*4 HSET key field value` packet parser routed before the generic borrowed argv parser, with the existing borrowed HSET runtime executor. All noncanonical shapes, multi-field HSET, limit-sensitive inputs, and malformed packets fell back to the existing parser.

## Baseline

- Built baseline with `rch`:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass173-baseline-target`
  - command: `cargo build --profile release-perf -p fr-server -p fr-bench`
- Independent baseline HSET P16/C50/n1M:
  - mean: `1.515587542s`
  - stddev: `0.046508473s`
  - last run throughput: `647118.53 ops/sec`

## Behavior Proof

- Focused parser tests passed with `rch`:
  - `cargo test -p fr-server borrowed_plain_hset_packet_parser -- --nocapture`
- Golden raw TCP transcript:
  - input SHA256: `b540e6966a2b0aa7457c417fe34d44d6d21506c0327dbf23ab913b18541e818e`
  - baseline output SHA256: `67fc60f37c1cb3ead3b4e3be5c5b0f1ab2f2f2a6cfef1cc9411a64308131efe1`
  - candidate output SHA256: `67fc60f37c1cb3ead3b4e3be5c5b0f1ab2f2f2a6cfef1cc9411a64308131efe1`
- Isomorphism:
  - Ordering/tie-breaking: the shortcut executed exactly one HSET per parsed packet and consumed one packet per loop iteration, so response order and serial side effects matched the generic path.
  - Error precedence: noncanonical, multi-field, malformed, and config-limit cases fell through to the generic parser/dispatcher.
  - Floating point: no FP paths touched.
  - RNG: no RNG/LFU sampling path touched.

## Re-benchmark

- Paired HSET P16/C50/n1M hyperfine:
  - baseline: `1.171917784s +/- 0.154146446s`
  - candidate: `1.123105141s +/- 0.097372780s`
  - hyperfine summary: candidate `1.04 +/- 0.16x` faster than baseline
- Last-run throughput from the paired benchmark logs:
  - baseline: `1033035.45 ops/sec`
  - candidate: `977840.04 ops/sec`
- Score:
  - `0.05 impact * 0.35 confidence / 1.0 effort = 0.0175`
  - Fails the required `Score>=2.0` keep gate.

## Decision

- Rejected.
- Production source hunk and candidate-only tests were removed before commit.
- Evidence retained in this directory.

## Next Route

Stop single-command parser shims for this HSET lane. Attack a true batch primitive next: parse repeated canonical `HSET key field value` packets from one readable buffer into a compact batch descriptor, execute them with batch-level command metadata/reply emission, and preserve per-command active-expire, stats, slowlog, output ordering, dirty propagation, and golden SHA256.
