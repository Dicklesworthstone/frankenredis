# frankenredis-8wkx3 Proof

Status: rejected.

## Target

Pass 63 targeted the fresh pass 62 pushed-main GET P16/1M profile:

- `<fr_store::Value>::string_owned`: 7.39% self
- `__memmove_avx_unaligned_erms`: 8.80% self
- `Runtime::refresh_store_runtime_info_context`: 2.97% self
- `fr_protocol::parse_command_args_borrowed_into`: 2.62% self

Fresh baseline on current source:

- GET P16/300k: `388.8 ms +/- 9.8 ms`

## Lever Tested

One lever was tested:

- Add a RESP2-only direct borrowed GET encoder.
- Preserve the existing Runtime GET fast-path bookkeeping: command count,
  session timestamps, runtime-info refresh, last command metadata, reply
  suppression, active-expire cycle, metrics, lazy-expiry propagation, read count,
  and error accounting.
- Encode GET hit/miss/wrong-type bytes directly into the connection write
  buffer instead of returning `RespFrame::BulkString(Some(Vec<_>))` for the
  default RESP2 borrowed GET path.
- RESP3 was deliberately left on the existing generic path so null semantics
  stayed exact.

The source hunk and candidate-only unit test were removed after measurement
because the lever failed the Score >= 2.0 keep threshold.

## Behavior Proof

Golden TCP transcript covered:

- `SET k v`
- GET hit
- GET miss
- `LPUSH l a`
- wrong-type `GET l`
- `QUIT`

SHA-256 matched exactly:

```text
b79d023e586718789382bba51c903392bb6ac4aed9dfa1405cd97bc151fe87ef  golden-baseline.resp
b79d023e586718789382bba51c903392bb6ac4aed9dfa1405cd97bc151fe87ef  golden-candidate.resp
```

Both outputs were 94 bytes, and `cmp` reported no byte differences.

Isomorphism notes:

- Ordering/tie-breaking: replies were byte-identical and in the same order; no
  ranked/set/list ordering logic changed.
- Floating point: N/A.
- RNG: no RNG path changed; LFU sample order in the GET path was preserved.
- RESP dialect: direct path claimed RESP2 only; RESP3 fell back to the existing
  encoder to preserve null behavior.
- Store state: lookup counters, touch, LFU bump, lazy expiry, and error stats
  matched the generic/runtime parity test while candidate was applied.

Validation while candidate was applied:

- `cargo fmt -p fr-store -p fr-runtime -p fr-server --check`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass63-check-target cargo check -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass63-runtime-test-target cargo test -p fr-runtime plain_get_borrowed_resp2_into_matches_generic_bytes_and_stats -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass63-clippy-target cargo clippy -p fr-server --all-targets -- -D warnings`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass63-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass63-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass63-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Baseline-only calibration:

- GET P16/300k: `388.8 ms +/- 9.8 ms`

Paired GET P16/300k:

- Baseline: `393.2 ms +/- 13.7 ms`
- Candidate: `402.4 ms +/- 27.5 ms`
- Baseline: `1.02x +/- 0.08x` faster than candidate

Reversed GET P16/1M confirmation:

- Candidate: `1.257 s +/- 0.038 s`
- Baseline: `1.288 s +/- 0.061 s`
- Candidate: `1.03x +/- 0.06x` faster than baseline

## Score

Score: `0 = Impact 0 x Confidence 1 / Effort 3`.

The 300k paired run was negative and the longer reversed run was inside noise.
This repeats the direct-GET-encoding family that also failed in pass 61, so the
right next move is not another reply-clone micro-variant.

## Next Route

Do not retry direct GET bulk-encoding variants. Reprofile first, then attack a
fundamentally different primitive:

- key/value layout that removes read cloning across all string-like reads,
- batch dispatch/output fusion, or
- persistent reply fragments / writev-style output.

Target at least `1.20x` before any keep.
