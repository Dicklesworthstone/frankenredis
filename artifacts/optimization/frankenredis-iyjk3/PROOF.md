# frankenredis-iyjk3 Proof

## Target

`frankenredis-iyjk3`: `[perf] TTL index active-expire primitive after SETEX profile`.

Profile basis came from the `frankenredis-svgvb` SETEX/PSETEX runs: TTL-heavy
writes moved the hot path into volatile-key active-expire/index work.

## Lever Tested

Candidate: add a `BTreeMap<u64, usize>` sidecar of expiry deadlines and skip
the active-expire BTree key walk while `now_ms` is before the earliest known
deadline.

This attacked the measured `Store::run_active_expire_cycle`, BTree range/iterator,
and `memcmp` rows without changing command parsing or borrowed dispatch.

## Behavior Proof

Golden transcript compared baseline and candidate binaries over:

- valid `SETEX` and `PSETEX`
- expiry-state proof through `PERSIST`
- lower/mixed-case command names
- invalid TTL fallback cases
- non-DB0 fallback
- `MULTI`/`EXEC` fallback

Result:

- Baseline SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Candidate SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Bytes: `992`

Isomorphism:

- RESP ordering and reply bytes: preserved by golden equality.
- TTL parse/overflow/fallback behavior: preserved by golden coverage.
- Tie-breaking/floating-point/RNG: unchanged; candidate touched only active-expire
  metadata and no score/RNG logic.
- Final production behavior: unchanged, because the rejected source hunk was
  removed.

## Validation

- Candidate release-perf build: `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-iyjk3-candidate-ttl-target cargo build --profile release-perf -p fr-server -p fr-bench` passed through RCH local fallback.
- RCH `cargo check -p fr-store --all-targets` passed on worker `vmi1167313`.
- RCH focused `cargo test -p fr-store sampling -- --nocapture` passed on worker
  `vmi1153651`.

## Benchmarks

Paired P16/300k alternating SETEX/PSETEX:

- Baseline: `1.4406747172342858 s +/- 0.16847744051520125 s`.
- Candidate: `1.5539561599485714 s +/- 0.15159803267453464 s`.
- Baseline was `1.08x +/- 0.16` faster.

Reversed P16/1M alternating SETEX/PSETEX:

- Candidate: `5.032145657780001 s +/- 0.4953834400314694 s`.
- Baseline: `4.33377837398 s +/- 0.16228795667408175 s`.
- Baseline was `1.16x +/- 0.12` faster.

## Decision

Rejected. Score `0.0`: the confirmation run favored baseline. No production
source hunk is retained.

Next route: the deadline sidecar adds write-path overhead that outweighs skipped
pre-deadline sampling. Attack a different primitive next: avoid O(N) logical
memory estimation in periodic sampling, or design a TTL index that preserves
cursor/propgation order without per-write BTreeMap sidecar updates.
