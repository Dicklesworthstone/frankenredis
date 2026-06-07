# frankenredis-direct-get-bulk-encode-pmo4h Proof

Status: rejected.

## Target

Pass 61 reprofiled current `main` at `e782ad8cd` after the range-argv parser
rejection. The fresh GET P16/300k baseline was:

- Hyperfine: `400.0 ms +/- 9.3 ms`
- GET P16/1M profile run: `814052.88 ops/sec`, p50 `297us`, p95 `396us`,
  p99 `606us`, p999 `1004us`
- Server profile: 714 samples, 0 lost

Top flat samples:

- `<fr_store::Value>::string_owned`: 8.70%
- `__memmove_avx_unaligned_erms`: 6.02%
- `frankenredis::process_buffered_frames`: 3.48%
- `<fr_runtime::Runtime>::refresh_store_runtime_info_context`: 2.23%
- `fr_protocol::parse_command_args_borrowed_into`: 0.98%

The parser-only lane was no longer the top profile target. The tested
alien/no-gaps primitive was zero-copy data-plane reply materialization for the
borrowed GET path.

## Lever Tested

One lever was tested:

- Add a borrowed string-view GET helper in `fr-store`.
- Add direct borrowed bulk-string encoding helpers in `fr-protocol`.
- Add `Runtime::execute_plain_get_borrowed_into`.
- Route exact borrowed GET frames in `fr-server` to encode directly into the
  connection write buffer, avoiding `Value::string_owned` and
  `RespFrame::BulkString(Some(Vec<_>))` materialization for string hits.

The source hunk and tests were removed after benchmarking because the lever
failed the Score >= 2.0 keep threshold.

## Behavior Proof

Golden TCP transcript covered:

- string GET hit
- missing GET under RESP2
- int-encoded string GET
- wrong-type GET
- `HELLO 3` followed by missing GET under RESP3 null encoding
- reply ordering through one TCP transcript

SHA-256 matched exactly:

```text
1da8120e3ba74621829d436f8008a95949b3acdcdab46a8ad337b7e69cfb23a2  golden-baseline.resp
1da8120e3ba74621829d436f8008a95949b3acdcdab46a8ad337b7e69cfb23a2  golden-candidate.resp
```

Both outputs were 252 bytes, and `cmp` reported no byte differences.

Isomorphism notes:

- Ordering/tie-breaking: pipelined replies were byte-identical and in the same
  order; no ranked/set/list ordering logic changed.
- Floating point: N/A.
- RNG: N/A for the default GET workload; the rejected helper preserved the
  existing LFU random-sample point when enabled.
- RESP dialect: RESP2 null bulk and RESP3 null were both covered.
- Error precedence: wrong-type GET still emitted the same error bytes.

Validation while the candidate was applied:

- `cargo fmt -p fr-store -p fr-runtime -p fr-server --check`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-direct-get-check-target cargo check -p fr-protocol -p fr-store -p fr-runtime -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-direct-get-test-target cargo test -p fr-runtime plain_get_borrowed_fast_path_matches_generic_hit_miss_stats -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-direct-get-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass61-profile-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-direct-get-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Baseline-only calibration:

- GET P16/300k: `400.0 ms +/- 9.3 ms`

Paired GET P16/300k:

- Baseline: `385.7 ms +/- 14.0 ms`
- Candidate: `382.9 ms +/- 12.4 ms`
- Candidate: `1.01x +/- 0.05x` faster than baseline

Reversed GET P16/1M confirmation:

- Candidate: `1.279 s +/- 0.039 s`
- Baseline: `1.274 s +/- 0.034 s`
- Baseline: `1.00x +/- 0.04x` faster than candidate

## Score

Score: `0 = Impact 0 x Confidence 1 / Effort 3`.

The direct single-command GET reply encoder was tied in short paired evidence
and tied/slower in the larger reversed confirmation. It failed the keep
threshold. Production source was restored to baseline; only this proof bundle
and bead bookkeeping are retained.

## Next Route

Do not retry single-command direct GET reply materialization. The profile says
reply/value movement matters, but the isolated helper did not move end-to-end
throughput. The next deeper primitive should attack output as a class: a
per-readable-batch reply slab/arena, scatter-gather/writev-style reply
fragments, or a batched output construction path that reduces memmove and
buffer growth across the whole pipelined command batch. Reprofile before
choosing between that and a key/value layout primitive.
