# frankenredis-1k5km Proof

Status: rejected.

## Target

Pass 59 followed the rejected active-expire zero-key fast return. The fresh GET
P16 profile still pointed at the borrowed parser/dispatch/store path:

- `<fr_store::Store>::get`: 10.62%
- `<fr_runtime::Runtime>::refresh_store_runtime_info_context`: 9.12%
- `foldhash::quality::RandomState::hash_one`: 4.79%
- `<fr_runtime::Runtime>::execute_plain_get_borrowed`: 4.51%
- `fr_protocol::parse_command_args_borrowed_into`: residual parser cost in the
  hot multibulk path.

Alien/no-gaps primitive: zero-copy framing with parser-produced command
specialization.

## Lever Tested

One lever was tested: parse the exact `GET key` multibulk shape directly in
`fr-protocol`, returning a borrowed key slice plus consumed offset before
`fr-server` allocates/materializes the generic borrowed argv vector.

Non-GET frames, runtime fast-path gate refusals, and generic fallback states
fell back to the existing borrowed argv path.

## Behavior Proof

Golden TCP transcript covered nil GET, SET fallback, lowercase GET, unknown
command fallback, GET arity error, and malformed GET-shaped protocol error.

SHA-256 matched exactly:

```text
b3976425e4b416c0261d0ac124b169726f5e15b44732a0fe0b19bbb0c81dc289  golden-baseline.resp
b3976425e4b416c0261d0ac124b169726f5e15b44732a0fe0b19bbb0c81dc289  golden-candidate.resp
```

Isomorphism notes:

- Ordering/tie-breaking: pipelined replies remained byte-identical and in the
  same order.
- Floating point: N/A.
- RNG: N/A.
- Error precedence: GET-shaped malformed frames used the same protocol errors
  in focused parser tests; non-GET and fallback states used the original parser.

Validation while the candidate was applied:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1k5km-check2-target cargo check -p fr-protocol -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1k5km-protocol-test-target cargo test -p fr-protocol parse_plain_get_borrowed -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1k5km-server-test-target cargo test -p fr-server process_buffered_frames -- --nocapture`
- `cargo fmt -p fr-server --check`

Note: `cargo fmt -p fr-protocol --check` is blocked on pre-existing rustfmt
drift in the older float conversion table; the rejected candidate source was
removed, so no `fr-protocol` formatting change is retained.

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1k5km-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1k5km-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Baseline-only calibration before editing:

- GET P16/300k: `473.6 ms +/- 51.8 ms`

Paired GET P16/300k:

- Baseline: `455.456 ms +/- 15.601 ms`
- Candidate: `446.564 ms +/- 11.660 ms`
- Candidate: `1.02x +/- 0.04x`

Reversed GET P16/1M:

- Candidate: `1.875937 s +/- 0.294895 s`
- Baseline: `2.588836 s +/- 0.191069 s`
- Candidate: `1.38x +/- 0.24x`

Paired GET P16/1M confirmation:

- Baseline: `1.497272 s +/- 0.043168 s`
- Candidate: `1.423444 s +/- 0.027919 s`
- Candidate: `1.05x +/- 0.04x`

Reversed GET P16/300k confirmation:

- Candidate: `459.659 ms +/- 13.732 ms`
- Baseline: `460.627 ms +/- 18.109 ms`
- Candidate: `1.00x +/- 0.05x`

## Score

Score: `0.75 = Impact 1 x Confidence 1.5 / Effort 2`.

The candidate failed the `>= 2.0` keep threshold. Source and tests were removed;
only this proof artifact and bead bookkeeping are retained.

## Next Route

Do not retry exact per-command parser stubs for GET. The deeper primitive is a
range-index argv parser: parse multibulk arguments into reusable byte ranges
instead of `&[u8]` references, allowing a real per-client/per-tick arena without
borrow-lifetime conflicts and without allocating a fresh borrowed argv vector per
frame.
