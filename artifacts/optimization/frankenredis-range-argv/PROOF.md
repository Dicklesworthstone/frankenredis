# frankenredis-range-argv-parser-0hc3p Proof

Status: rejected.

## Target

Pass 60 followed the rejected exact GET parser fast lane. The profile-backed
target was the same borrowed parser/dispatch/store family from pass 59:

- `<fr_store::Store>::get`: 10.62%
- `<fr_runtime::Runtime>::refresh_store_runtime_info_context`: 9.12%
- `foldhash::quality::RandomState::hash_one`: 4.79%
- `<fr_runtime::Runtime>::execute_plain_get_borrowed`: 4.51%
- residual `fr_protocol::parse_command_args_borrowed_into` cost

Alien/no-gaps primitive: region/arena memory management for the RESP data
plane. The tested lever replaced the per-frame `Vec<&[u8]>` borrowed argv
materialization with a reusable byte-range vector in the server hot loop.

## Lever Tested

One lever was tested:

- Add `BorrowedCommandArgRange { start, end }` and
  `parse_command_arg_ranges_into` to `fr-protocol`.
- In `fr-server::process_buffered_frames`, parse multibulk argv into a reused
  range vector and reconstruct borrowed slices only inside command fast-path
  matching or generic fallback copying.
- Preserve the existing borrowed fast-path command set and fallback behavior.

The source hunk was removed after benchmarking because the lever failed the
Score >= 2.0 keep threshold.

## Behavior Proof

Golden TCP transcript covered fixed fast paths (`SET`, `GET`, `INCR`,
`INCRBY`, `GETRANGE`, `STRLEN`, `LINDEX`), variable fast paths (`MGET`,
`EXISTS`, `HMGET`), generic fallback (`HSET`, `PING`), empty/null multibulk
handling, and final protocol error handling.

SHA-256 matched exactly:

```text
2d424af64b473ce3ddb1b2d2d5a2af2aa0d5b557338e11c61f3bf6ce15236354  golden-baseline.resp
2d424af64b473ce3ddb1b2d2d5a2af2aa0d5b557338e11c61f3bf6ce15236354  golden-candidate.resp
```

Both outputs were 136 bytes, and `cmp` reported no byte differences.

Isomorphism notes:

- Ordering/tie-breaking: pipelined replies were byte-identical and in the same
  order; no ranked/set/list ordering logic changed.
- Floating point: N/A.
- RNG: N/A.
- Error precedence: null bulk command args and non-command empty/null multibulks
  retained the baseline replies in the golden transcript.
- Fallback: non-fast-path commands still copied the same byte slices into the
  owned argv scratch before dispatch.

Validation while the candidate was applied:

- `cargo fmt -p fr-server --check`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-range-argv-check-target cargo check -p fr-protocol -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-range-argv-protocol-test-target cargo test -p fr-protocol parse_command_arg_ranges_into -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-range-argv-server-test-target cargo test -p fr-server process_buffered_frames -- --nocapture`

Known pre-existing validation blockers:

- `cargo fmt -p fr-protocol --check` is red on older fpconv table/function
  formatting outside this pass.
- `cargo clippy -p fr-protocol -p fr-server --all-targets -- -D warnings`
  is red on an unrelated pre-existing `nonminimal_bool` warning around
  `crates/fr-server/src/main.rs:253`.

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-range-argv-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-range-argv-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Baseline-only calibration:

- GET P16/300k: `439.0 ms +/- 40.2 ms`

Paired GET P16/300k:

- Baseline: `407.3 ms +/- 11.8 ms`
- Candidate: `408.7 ms +/- 18.4 ms`
- Baseline: `1.00x +/- 0.05x` faster than candidate

Reversed GET P16/1M confirmation:

- Candidate: `2.339 s +/- 1.126 s`
- Baseline: `1.327 s +/- 0.029 s`
- Baseline: `1.76x +/- 0.85x` faster than candidate

## Score

Score: `0 = Impact 0 x Confidence 1 / Effort 3`.

The candidate was tied or slower in same-window evidence, so it failed the keep
threshold. Production source was restored to baseline; only this proof bundle
and bead bookkeeping are retained.

## Next Route

Do not retry range-vector reconstruction in front of the current borrowed
fast-path chain. The next deeper primitive should avoid reconstructing argv in
the server loop entirely: either move command classification/tokenization into
the parser with a compact command token plus key/arg spans, or pivot to a
batched output/write path if the fresh profile shows reply/syscall costs above
parser materialization.
