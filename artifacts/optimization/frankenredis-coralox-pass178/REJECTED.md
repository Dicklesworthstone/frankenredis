# frankenredis-xcgry rejected: consecutive canonical HSET batch

## Target

- Bead: `frankenredis-xcgry`
- Parent lane: `frankenredis-tbmu1` / `frankenredis-zweth`
- Profile-backed route:
  - Post-`frankenredis-tbmu1` sweep still showed HSET as the main
    Redis-over-FR residual: Redis/fr `1.0280x`, FrankenRedis p99 `4819us`,
    Redis p99 `1975us`.
  - `frankenredis-zweth` rejected the remaining HSET value-slice allocation
    lever on confirmation, so this pass rerouted from micro-allocation tuning to
    parser/event-loop batching.

## Lever Tested

- Candidate recognized runs of consecutive canonical RESP
  `*4 HSET key field value` packets already present in the server read buffer.
- Each packet still executed through `Runtime::execute_plain_hset_borrowed`.
- Replies were encoded in command order, with CLIENT REPLY suppression, pub/sub
  drain, and output-limit checks after each packet.
- The candidate stopped at the first non-canonical, incomplete, or gated packet.
- Production source hunk was removed before commit after benchmark rejection.

## Behavior Proof

- RCH parser/unit gate passed:
  - `cargo test -p fr-server borrowed_plain_hset_packet_parser -- --nocapture`
- RCH compile gate passed:
  - `cargo check -p fr-server --all-targets`
- RCH candidate build passed:
  - `cargo build --profile release-perf -p fr-server -p fr-bench`
- Formatting check passed:
  - `rustfmt --edition 2024 --check crates/fr-server/src/main.rs`
- `ubs crates/fr-server/src/main.rs` reported pre-existing broad findings, but
  its fmt/clippy/build sections were clean for this candidate.
- Raw TCP HSET/HGET/HGETALL golden:
  - input SHA256:
    `07a2d97c8bc906bad830fd87ff1bd2ce407975d3dbcf20153ae4f566ab70f40a`
  - baseline output SHA256:
    `ea34d540dad6c1176557492ae693c76d79fde38df8aebd130df1bdecf029b280`
  - candidate output SHA256:
    `ea34d540dad6c1176557492ae693c76d79fde38df8aebd130df1bdecf029b280`
- Isomorphism checklist:
  - Ordering/tie-breaking: per-packet execution and reply encoding stayed in
    read-buffer order.
  - CLIENT REPLY/pubsub/output limits: candidate used the same suppression,
    pub/sub drain, and hard output-limit decisions after every packet.
  - Wrongtype/dirty/LFU/RNG: unchanged because commands still reached the same
    `Runtime::execute_plain_hset_borrowed` path.
  - Floating point: no FP path touched.

## Benchmarks

- Baseline build:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass178-current-target`
  - `cargo build --profile release-perf -p fr-server -p fr-bench` via `rch`
- Candidate build:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass178-candidate-target`
  - `cargo build --profile release-perf -p fr-server -p fr-bench` via `rch`
- Independent HSET P16/C50/n3M baseline:
  - `3.83103676628s +/- 0.10695485428063696s`
- Paired HSET P16/C50/n3M:
  - baseline `3.3181927112199996s +/- 0.12379197319359943s`
  - candidate `3.0602741486485714s +/- 0.10125465683294443s`
  - hyperfine summary: candidate `1.08 +/- 0.05x` faster
  - last-run throughput: baseline `897537.88 ops/sec`, candidate
    `951439.92 ops/sec`
  - last-run p99: baseline `1481us`, candidate `1441us`
- Confirmation HSET P16/C50/n10M:
  - baseline `9.58852987358s +/- 0.23008825118525889s`
  - candidate `9.90405633458s +/- 0.6722255306564718s`
  - hyperfine summary: baseline `1.03 +/- 0.07x` faster
  - last-run throughput: baseline `1026546.67 ops/sec`, candidate
    `986783.32 ops/sec`
  - last-run p99: baseline `1422us`, candidate `1455us`

## Score

- Impact: `0` for keep/reject purposes; the longer confirmation regressed.
- Confidence: `4.0` that this lever is not worth shipping.
- Effort: `1.5`
- Score: `0 * 4.0 / 1.5 = 0`

## Decision

- Rejected under the Score>=2.0 rule.
- Production source hunk was removed before commit.
- Evidence retained in this directory.

## Next Route

- Do not repeat canonical HSET read-loop batching in this form.
- The next deeper primitive should attack zero-copy RESP frame scanning with an
  arena/slab-backed request representation or branchless command dispatch, with
  a fresh profile-backed baseline.
