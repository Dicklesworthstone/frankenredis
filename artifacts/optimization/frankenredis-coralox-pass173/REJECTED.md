# frankenredis-ohsk5.48 rejected: INCR one-probe expiry/mutation fusion

## Target

- Bead: `frankenredis-ohsk5.48`
- Fresh post-pass172 dashboard on current main selected INCR as the top
  remaining P16/C50 residual:
  - Redis: `877192 req/s`
  - FrankenRedis: `763358 req/s`
  - Redis/fr ratio: `1.15x`
- `perf_event_paranoid=4` blocked kernel perf. The profile-backed userspace
  target came from child-owned GDB under INCR load:
  `__memcmp_avx2_movbe -> hashbrown lookup -> Store::drop_if_expired ->
  incrby_existing_or_insert -> execute_plain_incr_borrowed`.
- Syscall profile confirmed the expected P16 floor:
  `epoll_wait` 41.84%, `sendto` 29.82%, `recvfrom` 13.32%.
- This pass tested one lever only: fold the existing-key expiry check into the
  `entries.get_mut` mutation path so live, non-expired INCR keys avoid a
  second key probe/comparison.

## Baseline

- Built current main release-perf with `rch`:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass173-current-target`
  - command: `cargo build --profile release-perf -p fr-server -p fr-bench`
- Independent INCR P16/C50/n1M:
  - throughput: `772194.42 ops/sec`
  - p50: `920us`
  - p95: `1356us`
  - p99: `1956us`
  - elapsed: `1295ms`
- Independent hyperfine INCR P16/C50/n1M:
  - baseline: `1.659s +/- 0.120`

## Behavior Proof

- Focused store tests passed with `rch`:
  - `cargo test -p fr-store incrby_ -- --nocapture`
- Store check passed with `rch`:
  - `cargo check -p fr-store --all-targets`
- Release-perf candidate build passed with `rch`:
  - `cargo build --profile release-perf -p fr-server -p fr-bench`
- Local source hygiene:
  - `cargo fmt --package fr-store -- --check`
  - `git diff --check -- crates/fr-store/src/lib.rs`
- Golden raw TCP transcript:
  - input SHA256: `704ada749865b72dd5de075fa720617118ad0006b188d7352d19928ce04f74a4`
  - baseline output SHA256: `27f354aa4120bbee6dac946839795e2066a7c2b55ea18b3f824b27e3f22aaeba`
  - candidate output SHA256: `27f354aa4120bbee6dac946839795e2066a7c2b55ea18b3f824b27e3f22aaeba`
  - output size: `253` bytes for both baseline and candidate
- Isomorphism:
  - Ordering/tie-breaking: the lever changed only how the existing key entry
    was reached before the same INCR mutation; command order and reply order
    remained serial and byte-identical.
  - Error precedence: wrongtype, invalid integer, overflow, expired key, and
    missing-key behavior were covered by the focused golden and existing
    `incrby_` tests.
  - Floating point: no FP paths touched.
  - RNG: no RNG/LFU sampling path touched.

## Re-benchmark

- Paired INCR P16/C50/n1M hyperfine:
  - baseline: `1.731s +/- 0.030`
  - candidate: `1.692s +/- 0.042`
  - hyperfine summary: candidate `1.02x +/- 0.03` faster than baseline
- Reversed-order INCR P16/C50/n2M hyperfine:
  - candidate-first: `2.815s +/- 0.134`
  - baseline-second: `2.774s +/- 0.141`
  - hyperfine summary: baseline `1.01x +/- 0.07` faster than candidate
- Score:
  - `0.4 impact * 2.0 confidence / 1.0 effort = 0.8`
  - Fails the required `Score>=2.0` keep gate.

## Decision

- Rejected.
- Production source hunk and candidate-only test were removed before commit.
- Evidence retained in this directory.

## Next Route

Stop one-probe `drop_if_expired` / existing-key INCR microlevers. Re-profile
current main and choose a deeper primitive: command-batch metadata, key/probe
layout, arena-backed command packets, or pivot to the fresh SADD residual if it
remains the top measured gap.
