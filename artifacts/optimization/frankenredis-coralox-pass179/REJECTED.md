# frankenredis-a9cdm rejected: exact RESP packet header routing

## Target

- Bead: `frankenredis-a9cdm`
- Parent lane: `frankenredis-tbmu1` / `frankenredis-zweth` /
  `frankenredis-xcgry`
- Profile-backed route:
  - Post-`frankenredis-tbmu1` sweep still showed HSET as the measured residual:
    Redis/fr `1.0280x`, FrankenRedis p99 `4819us`, Redis p99 `1975us`.
  - HSET value-slice allocation and consecutive-HSET read-loop batching both
    failed confirmation, so this pass tested a different frame-scanning lever.

## Lever Tested

- Candidate added a small exact-packet classifier for canonical RESP
  GET/SET/HSET packet headers before the existing exact packet parsers.
- Intended effect: route directly to the matching parser and avoid wrong-parser
  probes on HSET, while leaving all parser/runtime fast paths unchanged.
- Production source hunk was removed before commit after benchmark rejection.

## Behavior Proof

- Formatting passed:
  - `rustfmt --edition 2024 --check crates/fr-server/src/main.rs`
- RCH parser/unit gate passed:
  - `cargo test -p fr-server borrowed_plain_hset_packet_parser -- --nocapture`
- RCH compile gate passed:
  - `cargo check -p fr-server --all-targets`
- RCH candidate build passed:
  - `cargo build --profile release-perf -p fr-server -p fr-bench`
- Raw TCP HSET/HGET/HGETALL golden:
  - input SHA256:
    `07a2d97c8bc906bad830fd87ff1bd2ce407975d3dbcf20153ae4f566ab70f40a`
  - baseline output SHA256:
    `ea34d540dad6c1176557492ae693c76d79fde38df8aebd130df1bdecf029b280`
  - candidate output SHA256:
    `ea34d540dad6c1176557492ae693c76d79fde38df8aebd130df1bdecf029b280`
- Isomorphism checklist:
  - Ordering/tie-breaking: unchanged; accepted packets still dispatch through
    the same single-command parser/runtime path.
  - Parser errors/fallbacks: non-canonical, incomplete, and limited inputs fell
    through to the existing generic parser path.
  - Wrongtype/dirty/LFU/RNG: unchanged because runtime fast paths were not
    modified.
  - Floating point: no FP path touched.

## Benchmarks

- Baseline build:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass179-current-target`
  - `cargo build --profile release-perf -p fr-server -p fr-bench` via `rch`
- Candidate build:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass179-candidate-target`
  - `cargo build --profile release-perf -p fr-server -p fr-bench` via `rch`
- Independent HSET P16/C50/n3M baseline:
  - `3.6295800352600005s +/- 0.2099832525695219s`
- Paired HSET P16/C50/n3M:
  - baseline `3.351531751877143s +/- 0.12433060057625261s`
  - candidate `3.7797286038771434s +/- 0.09873727947825983s`
  - hyperfine summary: baseline `1.13 +/- 0.05x` faster
  - last-run throughput: baseline `881255.93 ops/sec`, candidate
    `810836.20 ops/sec`
  - last-run p99: baseline `1852us`, candidate `1741us`

## Score

- Impact: `0` for keep/reject purposes; paired hyperfine and throughput
  regressed.
- Confidence: `4.0` that this lever is not worth shipping.
- Effort: `1.0`
- Score: `0 * 4.0 / 1.0 = 0`

## Decision

- Rejected under the Score>=2.0 rule.
- Production source hunk was removed before commit.
- Evidence retained in this directory.

## Next Route

- Do not repeat header-routing or wrong-parser-probe micro-levers.
- The next primitive should move deeper than exact parser dispatch: zero-copy
  RESP frame scanning with an arena/slab-backed request representation, or
  branchless command dispatch after a fresh profile-backed target.
