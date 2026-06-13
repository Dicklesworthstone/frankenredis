# frankenredis-zweth rejected: HSET borrowed value slice path

## Target

- Bead: `frankenredis-zweth`
- Parent lane: `frankenredis-tbmu1`
- Profile-backed route:
  - Post-`frankenredis-tbmu1` sweep showed HSET remained the main Redis-over-FR
    residual: Redis/fr `1.0280x`, FrankenRedis p99 `4819us`, Redis p99
    `1975us`.
  - Historical `ohsk5` / `gu5nf` profiles kept allocator pressure and
    per-command allocation as first-class HSET residuals.

## Lever Tested

- Candidate added a borrowed-value slice path:
  - `Runtime::execute_plain_hset_borrowed`
  - `Store::hset_borrowed_slices`
  - `HashFieldMap::insert_slices`
  - `PackedStrMap::insert_slices`
- Intended effect: avoid `pair[1].to_vec()` before packed duplicate-field HSET
  overwrites when the value bytes already live in the RESP input buffer.

## Behavior Proof

- RCH property gate passed:
  - `cargo test -p fr-store map_equivalent_to_indexmap -- --nocapture`
  - The property was temporarily extended to compare `insert_slices` against
    `IndexMap` for insert, overwrite, remove, get, contains, length, and
    iteration order.
- RCH compile gate passed:
  - `cargo check -p fr-runtime --all-targets`
  - Warnings were pre-existing and fixed upstream by `aef30f215`; no candidate
    compiler issue appeared.
- Raw TCP HSET/HGET/HGETALL golden:
  - input SHA256:
    `07a2d97c8bc906bad830fd87ff1bd2ce407975d3dbcf20153ae4f566ab70f40a`
  - baseline output SHA256:
    `ea34d540dad6c1176557492ae693c76d79fde38df8aebd130df1bdecf029b280`
  - candidate output SHA256:
    `ea34d540dad6c1176557492ae693c76d79fde38df8aebd130df1bdecf029b280`
- Isomorphism checklist:
  - Ordering/tie-breaking: candidate kept packed record position on overwrite
    and append order on insert.
  - Wrongtype/dirty/LFU/RNG: candidate mirrored `Store::hset_borrowed` before
    the map write.
  - Floating point: no FP path touched.

## Benchmarks

- Baseline build:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass177-current-target`
  - `cargo build --profile release-perf -p fr-server -p fr-bench` via `rch`
- Candidate build:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass177-candidate-target`
  - `cargo build --profile release-perf -p fr-server -p fr-bench` via `rch`
- Independent HSET P16/C50/n3M baseline:
  - `3.160s +/- 0.162s`
  - last run `955505.89 ops/sec`, p99 `1498us`
- Paired HSET P16/C50/n3M:
  - baseline `3.825120412285714s +/- 0.3549858860100393s`
  - candidate `3.6030313725714294s +/- 0.18762380758520641s`
  - hyperfine summary: candidate `1.06 +/- 0.11x` faster
  - last-run throughput: baseline `994693.99 ops/sec`, candidate
    `997993.21 ops/sec`
  - last-run p99: baseline `1404us`, candidate `1295us`
- Confirmation HSET P16/C50/n10M:
  - baseline `9.939359501400002s +/- 0.6424591760556899s`
  - candidate `9.8073556266s +/- 0.5167850949678012s`
  - hyperfine summary: candidate `1.01 +/- 0.08x` faster
  - last-run throughput: baseline `1150483.85 ops/sec`, candidate
    `1058590.24 ops/sec`
  - last-run p99: baseline `1067us`, candidate `1277us`

## Score

- Impact: `0` for keep/reject purposes; the longer confirmation did not prove a
  real win.
- Confidence: `4.0` that this lever is not worth shipping.
- Effort: `1.5`
- Score: `0 * 4.0 / 1.5 = 0`

## Decision

- Rejected under the Score>=2.0 rule.
- Production source hunk was removed before commit.
- Evidence retained in this directory.

## Next Route

- Do not repeat HSET value-slice micro-allocation variants.
- The next deeper primitive should target parser/batch allocation structure or a
  different HSET store-layout path with fresh profile support.
