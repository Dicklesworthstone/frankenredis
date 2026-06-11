# frankenredis-6zalg pass1 rejection record

## Target

- Bead: `frankenredis-6zalg`
- Candidate: expiration-free fast path for `Store::get_string_bytes` when `expires_count == 0`, using one mutable keyspace lookup instead of `record_keyspace_lookup` followed by `entries.get_mut`.
- Profile basis: the fresh GET/P16/C50/3M profile kept `<fr_store::Store>::get_string_bytes` at 2.60% self, with children in `drop_if_expired` and `__memcmp_avx2_movbe`.
- Alien mapping: key/probe layout reduction. The candidate removes a repeated hash/probe only in the persistent-key shape rather than changing command semantics.

## Behavior proof

- Focused candidate tests passed remotely:
  `rch exec -- cargo test -p fr-store get_string_bytes -- --nocapture`
- Candidate crate gates passed:
  `cargo check -p fr-store --all-targets` (green with pre-existing test-target warnings)
  `cargo clippy -p fr-store --lib -- -D warnings`
  `cargo fmt -p fr-store -- --check`
- Golden TCP transcript covered GET hit, GET miss, wrong-type GET, past-expired key via `PEXPIREAT`, RESP3 nil, and QUIT.
- Request sha256: `0cf4494a4d4a911e2134548645a6fb0e52b38a7133b9730bab40008f566209aa`
- Current response sha256: `6430c62cb8dbaba71289812f9cabaf9714a4d2717565c2f14a3fb81f63d46752`
- Candidate response sha256: `6430c62cb8dbaba71289812f9cabaf9714a4d2717565c2f14a3fb81f63d46752`
- Ordering, tie-breaking, floating-point behavior, and RNG behavior were unchanged. The focused LFU test covered missing, expired, wrong-type, and hit RNG consumption shape.

## Performance

- Baseline build: `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`.
- Baseline GET/P16/C50/1M current-only hyperfine: `763.3 ms +/- 47.9 ms`; harness last-run `1,510,416 ops/sec`, p95 `768us`, p99 `1014us`.
- Paired GET/P16/C50/1M:
  - Current: `700.9 ms +/- 44.6 ms`
  - Candidate: `777.7 ms +/- 58.0 ms`
  - Hyperfine summary: current `1.11x +/- 0.11` faster
  - Harness last-run current: `1,646,670 ops/sec`, p95 `716us`, p99 `925us`
  - Harness last-run candidate: `1,626,221 ops/sec`, p95 `689us`, p99 `895us`
- Reversed GET/P16/C50/1M:
  - Candidate: `763.4 ms +/- 34.6 ms`
  - Current: `785.0 ms +/- 78.1 ms`
  - Hyperfine summary: candidate `1.03x +/- 0.11` faster
  - Harness last-run candidate: `1,591,985 ops/sec`, p95 `700us`, p99 `918us`
  - Harness last-run current: `1,540,838 ops/sec`, p95 `792us`, p99 `1233us`

## Decision

Rejected under the Score>=2.0 keep gate.

The paired order favors current, while the reversed order only weakly favors candidate inside noise. The candidate is correct, but not a reproducible win.

Score estimate: Impact `0.0` x Confidence `4.0` / Effort `2.0` = `0.0`.

The candidate source hunk was never applied to shared `main`; only this evidence bundle and the Beads close remain.

## Next route

Do not repeat local `get_string_bytes` lookup micro-shapes. Re-profile current main after the active peer-owned lanes land, then attack a larger primitive: key hash/fingerprint layout across GET execution, timing/accounting amortization, or a server/output primitive that composes with the current GET batching work.
