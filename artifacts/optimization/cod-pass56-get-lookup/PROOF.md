# frankenredis-xssbh - GET lookup fusion rejection

Status: rejected, source not kept.

## Profile-backed target

Server-side `perf report` for GET P16 / 1M requests showed:

- `__memcmp_avx2_movbe`: 15.00%
- `Store::drop_if_expired`: 14.72%
- `Runtime::refresh_store_runtime_info_context`: 10.99%
- `execute_plain_get_borrowed`: 3.36%
- `parse_command_args_borrowed_into`: 2.00%

Evidence:

- `artifacts/optimization/cod-pass56-profile-20260607T0512Z/current-get-p16-1m-server-perf-report.txt`
- `artifacts/optimization/cod-pass56-profile-20260607T0512Z/current-get-p16-1m-server-profile-run.json`

## Lever tested

Candidate changed `Store::get` to bypass `record_keyspace_lookup` when:

- `volatile_keys.is_empty()`
- LFU tracking is disabled

The candidate directly used one mutable `entries.get_mut(key)` lookup, manually preserved
hit/miss counters, returned `WRONGTYPE` before touch, and touched string hits.

The source hunk was rejected and removed after benchmark.

## Behavior proof

Golden TCP transcript covered:

- `SET` + `GET` present string
- `GET` missing key
- wrong-type `GET` against a list
- TTL-bearing `SET PX` + `GET`

Baseline and candidate transcript SHA-256:

```text
bfc4a4b15cf9000ca55cb2a9c9988c90aab1bd45a5ed2aebb4cb3f385174f4a9  artifacts/optimization/cod-pass56-get-lookup/baseline-golden.resp
bfc4a4b15cf9000ca55cb2a9c9988c90aab1bd45a5ed2aebb4cb3f385174f4a9  artifacts/optimization/cod-pass56-get-lookup/candidate-golden.resp
```

Isomorphism notes:

- Ordering/tie-breaking/floating-point behavior: not applicable to this GET path.
- RNG behavior: LFU-tracking state fell back to the old path, preserving random draw count.
- TTL behavior: any non-empty volatile key index fell back to the old path.
- Wrong-type behavior: candidate returned `WRONGTYPE` before touching the object.
- Statistics: hit/miss counters were manually preserved in the candidate and covered by a unit test.

Validation run before rejection:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-xssbh-store-check-target cargo check -p fr-store --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-xssbh-store-clippy-target cargo clippy -p fr-store --all-targets -- -D warnings`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-xssbh-store-test-target-a cargo test -p fr-store get_without_volatile_keys_preserves_stats_wrongtype_and_lru -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-xssbh-store-test-target-b cargo test -p fr-store keyspace_hit_and_miss_counters_follow_store_lookup_paths -- --nocapture`

## Benchmark result

Clean baseline worktree:

- `/data/projects/.scratch/frankenredis-xssbh-baseline-20260607T0521`
- commit `50d79b883bef60b6e54ac0716ff08ef16fa96f56`
- binary SHA: `6aad681a6bd63bb3b6bf79d8a352503ce7e5bf4b703a5b539d68403d90feb134`

Clean candidate worktree:

- `/data/projects/.scratch/frankenredis-xssbh-candidate-20260607T0532`
- base commit `50d79b883bef60b6e54ac0716ff08ef16fa96f56`
- store-only candidate binary SHA: `ee0a388ff9aa6771e7f17770aa389d8d6826662e65cd1f913df29f8575ee2e78`

Paired GET P16 / 300k hyperfine:

- baseline: `0.61857117932s +/- 0.09898182453s`
- candidate: `0.67181999182s +/- 0.08585505238s`
- result: baseline `1.09x +/- 0.22x` faster

Reversed-order GET P16 / 1M hyperfine:

- candidate: `1.59342443452s +/- 0.06432465098s`
- baseline: `1.59694004182s +/- 0.14582546771s`
- result: candidate `1.00x +/- 0.10x` faster

Decision:

- Impact: 0, because same-worker evidence showed no reliable win.
- Confidence: high enough to reject this exact lever after the order-flip check.
- Effort: low, but score remains below the `>=2.0` keep gate.

Next primitive:

- Do not retry one-off `Store::get` lookup fusion.
- Attack the larger server-profile sources instead: command-name/context refresh work,
  parser/command memcmp elimination, or active-expire/time sampling in the borrowed GET loop.
