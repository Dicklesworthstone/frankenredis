# frankenredis-a971j rejection proof

## Target

- Bead: `frankenredis-a971j`
- Profile-backed target: `fr_store::estimate_entry_memory_usage_bytes` was 9.90% flat in the SETEX/PSETEX P16/1M profile captured for `frankenredis-svgvb`; most sampled cost was under `Store::record_ops_sec_sample`.
- Lever tested: defer logical memory estimation in `Store::record_ops_sec_sample` by calling `read_rss_bytes()` first and falling back to `estimate_memory_usage_bytes()` only when RSS is unavailable.
- Source state after rejection: candidate hunk removed; no source change from this lever is kept.

## Behavior isomorphism

- Command replies, error replies, transaction queueing/EXEC ordering, DB selection, expiration/PERSIST behavior, and case-insensitive command dispatch were checked with the existing SETEX/PSETEX golden transcript.
- The lever only changed periodic sampling internals. It did not change key insertion order, expiration ordering, tie-breaking, floating-point behavior, or RNG usage.
- Golden transcript:
  - baseline SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
  - candidate SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
  - bytes: 992 vs 992
  - equal: true
  - artifact: `artifacts/optimization/frankenredis-a971j/golden-compare.json`

## Validation

- `cargo fmt -p fr-store --check` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-a971j-check-target cargo check -p fr-store --all-targets` passed on worker `vmi1153651`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-a971j-test-target cargo test -p fr-store periodic_sampling_updates_rss_and_peak_memory_stats -- --nocapture` passed via RCH local fallback.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-a971j-clippy-target cargo clippy -p fr-store --all-targets -- -D warnings` passed via RCH local fallback.

## Benchmarks

Baseline binary: `/tmp/codex-fr-1cbca-closeout-target2/release-perf/frankenredis`

Candidate binary: `/tmp/codex-fr-a971j-candidate-target/release-perf/frankenredis`

Workload: SETEX/PSETEX alternating, 50 clients, pipeline 16, keyspace 10000, datasize 3.

### Paired P16/300k

- Baseline: `1.488925604s +/- 0.121177189s`
- Candidate: `1.468956161s +/- 0.120989000s`
- Hyperfine summary: candidate `1.01x +/- 0.12` faster
- Artifact: `artifacts/optimization/frankenredis-a971j/a971j-setex-p16-300k-paired-hyperfine.json`

### Reversed P16/1M

- Candidate: `4.447081806s +/- 0.270821255s`
- Baseline: `4.357572499s +/- 0.108595125s`
- Hyperfine summary: baseline `1.02x +/- 0.07` faster
- Artifact: `artifacts/optimization/frankenredis-a971j/a971j-setex-p16-1m-reversed-hyperfine.json`

## Decision

Rejected. The candidate did not produce a credible same-host win and scores below the `Impact x Confidence / Effort >= 2.0` keep gate. The next route should avoid this sampling micro-lever family and attack a different profile-backed primitive.
