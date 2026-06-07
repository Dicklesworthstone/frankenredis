# frankenredis-get-store-fast-read-5rpzl Proof

Status: rejected.

## Target

Fresh server-side GET P16/1M profile on the baseline build still showed the
borrowed-read/store path as the dominant profile-backed family:

- `<fr_store::Store>::get`: 10.62%
  - `<fr_store::Store>::drop_if_expired`: 4.97%
  - `__memcmp_avx2_movbe`: 4.42%
  - `clock_gettime` via `execute_plain_get_borrowed`: 1.10%
- `<fr_runtime::Runtime>::refresh_store_runtime_info_context`: 9.12%
- `foldhash::quality::RandomState::hash_one`: 4.79%
- `<fr_runtime::Runtime>::execute_plain_get_borrowed`: 4.51%

Artifacts:

- `current-get-p16-1m-server.perf.data`
- `current-get-p16-1m-server-perf-report.txt`
- `current-get-p16-1m-profile-run.json`

## Lever Tested

One runtime lever was tested: skip the active-expire store scan when the runtime
can prove there are zero expiring keys, returning zero sampled/evicted stats
before cloning the active-expire cursor, entering `Store::run_active_expire_cycle`,
or starting the expiry-cycle timer.

The lever was intentionally separate from pass 56's rejected `Store::get`
lookup fusion and pass 57's rejected GET context-refresh skip.

## Behavior Proof

Golden TCP transcript exercised persistent GET plus a short-lived TTL key so the
candidate covered both no-expiry and TTL-bearing states.

SHA-256 matched exactly:

```text
3f4dda84862d9f15c69f8f43ab87f3421030f624494f1b25fb0a9c8f8f482063  golden-baseline.resp
3f4dda84862d9f15c69f8f43ab87f3421030f624494f1b25fb0a9c8f8f482063  golden-candidate.resp
```

Isomorphism notes:

- Ordering/tie-breaking: GET and expiry side effects retain command order; the
  fast return was only for the zero-expiring-key state and does not reorder DEL
  propagation for TTL-bearing keys.
- Floating point: N/A.
- RNG: N/A.
- Expiry: TTL-bearing states still fall through to the existing
  `Store::run_active_expire_cycle` path; the zero-expiring-key state has no keys
  available to sample or evict.

Validation while the candidate was applied:

- `cargo fmt -p fr-runtime --check`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-get-store-fast-read-runtime-check-target cargo check -p fr-runtime --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-get-store-fast-read-runtime-clippy-target cargo clippy -p fr-runtime --all-targets -- -D warnings`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-get-store-fast-read-runtime-test-target cargo test -p fr-runtime active_expire -- --nocapture`

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-get-store-fast-read-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-get-store-fast-read-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Baseline-only calibration before editing:

- GET P16/300k: `472.7 ms +/- 47.2 ms`

Paired GET P16/300k after candidate:

- Baseline: `591.3 ms +/- 119.8 ms`
- Candidate: `608.9 ms +/- 137.9 ms`
- Decision: baseline was `1.03x +/- 0.31x` faster.

Reversed GET P16/1M after candidate:

- Candidate: `1.667 s +/- 0.217 s`
- Baseline: `1.905 s +/- 0.227 s`
- Decision: candidate was `1.14x +/- 0.20x` faster, but this conflicts with the
  paired 300k result and is too noisy to justify keeping the source.

## Score

Score: `0.5 = Impact 1 x Confidence 1 / Effort 2`.

The candidate failed the `>= 2.0` keep threshold. Source changes were removed;
only this proof artifact and bead bookkeeping are retained.

## Next Route

Do not retry zero-expiry active-expire fast returns for the GET P16 path. The
fresh profile points to a deeper structural primitive instead: remove repeated
command/key classification work from the borrowed-read loop with an interned
command token or parser-produced command kind that preserves RESP ordering and
falls back for ambiguous frames.
