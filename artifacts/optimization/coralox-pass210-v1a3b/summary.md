# Pass 210 Summary - `frankenredis-v1a3b`

## Decision

Kept. Raise `DIRECT_OWNED_SET_CHUNK` from `64 KiB` to `256 KiB` for direct owned large `SET` staging reads. The generic `LARGE_CHUNK` fallback remains `64 KiB`.

## Profile-Backed Target

The corrected large-value gate showed the residual large-SET gap on current `1fc7cf304e5dccc4040b020458df52df16450387`:

- `SET 262144B`: FrankenRedis `12948 op/s`, Redis `20320 op/s`, ratio `0.64x`.
- `SET 1048576B`: FrankenRedis `3319 op/s`, Redis `4695 op/s`, ratio `0.71x`.
- GET rows were already faster than Redis at all sizes.

## Baseline And Candidate

Baseline binaries:

```text
frankenredis_sha256 1c4113b010533d39e00b741e4b008850c87cb9ad4c5a3952ee1bfe2a40c5d54f
fr_bench_sha256 7bfe8c307b290fce8af0db5a36bbfae26e706f9772960ba9d7405c1a6d1cabb7
redis_sha256 e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7
```

Candidate binaries:

```text
frankenredis_sha256 52cb561a6e73be4387e0bfb8a7a79be6755cf59fd6192df8165ca2d4762d9bf0
fr_bench_sha256 4d26377afc3dcde7e0d0f4ca780bbd2e1f3dba4c5ba1a041db3005ad61973c1b
redis_sha256 e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7
```

Candidate gate:

- `SET 262144B`: FrankenRedis `17969 op/s`, Redis `17563 op/s`, ratio `1.02x`.
- `SET 1048576B`: FrankenRedis `4217 op/s`, Redis `3977 op/s`, ratio `1.06x`.
- One noisy row remained below threshold: `SET 65536B` ratio `0.89x`.

Paired same-Redis confirmation:

- `SET 65536B`: baseline mean `43713.0 op/s`, candidate mean `45603.5 op/s`, ratio `1.043x`.
- `SET 262144B`: baseline mean `13041.5 op/s`, candidate mean `18306.5 op/s`, ratio `1.404x`.
- `SET 1048576B`: baseline mean `3646.5 op/s`, candidate mean `3825.0 op/s`, ratio `1.049x`.

Paired artifact sha256:

```text
73fc01bc4d9702376c4cb6a5be31f0e0ee6c144ecf734cf313d20151925ea550  artifacts/optimization/coralox-pass210-v1a3b/paired-large-value-gate.txt
```

## Behavior Proof

Split-send raw RESP proof covered 64KiB, 256KiB, and 1MiB `SET` / `STRLEN` / `GET` sequences through Redis, baseline, and candidate.

```text
request_sha256 d507f6a6c6958d7f175f9332aa104de75859f6be99f3557d5c041f42451216f5
redis_response_sha256 ee784c22fa4ad7cd4432d1ee0e83c41a2e30b7e148a1e5e06b702c7af25d505a
baseline_response_sha256 ee784c22fa4ad7cd4432d1ee0e83c41a2e30b7e148a1e5e06b702c7af25d505a
candidate_response_sha256 ee784c22fa4ad7cd4432d1ee0e83c41a2e30b7e148a1e5e06b702c7af25d505a
redis_baseline_cmp match
redis_candidate_cmp match
```

Isomorphism: only the maximum bytes read per nonblocking direct owned-SET staging read changes. RESP parsing, command ordering, replies, stored bytes, TTL/persistence surfaces, tie-breaking, floating-point, and RNG paths are unchanged.

## Gates

Passed:

- RCH release-perf baseline build: `cargo build --profile release-perf -p fr-server -p fr-bench`.
- RCH release-perf candidate build: `cargo build --profile release-perf -p fr-server -p fr-bench`.
- RCH focused tests: `cargo test -j 1 -p fr-server large_plain_set_read_start -- --nocapture`.
- RCH check: `cargo check -j 1 -p fr-server --all-targets`.
- RCH clippy: `cargo clippy -j 1 -p fr-server --all-targets -- -D warnings`.
- Local fmt: `cargo fmt -p fr-server -- --check`.
- Local whitespace: `git diff --check`.

UBS: `ubs crates/fr-server/src/main.rs` remains nonzero on pre-existing whole-file inventory; its embedded fmt/clippy/check/test-build sections were clean.

## Score

Target-row geomean: `sqrt(1.404 * 1.049) = 1.214`.

Score: `1.214 * 0.80 / 0.30 = 3.24`.

## Next Route

Re-profile current pushed main and select the next live `[perf]` bead. Do not repeat static chunk-size tuning without a fresh large-value profile row.
