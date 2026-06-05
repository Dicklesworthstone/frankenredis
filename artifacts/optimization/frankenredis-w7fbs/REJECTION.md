# frankenredis-w7fbs Rejection

## Lever Tested

`frankenredis-w7fbs` tested a borrowed-command-only rewrite of
`parse_command_len_strict`:

- preserve `-1` null length handling
- preserve leading-zero rejection
- preserve `i64::MAX` length boundary semantics
- preserve owned-parser error parity for array and bulk lengths
- replace per-digit checked arithmetic with a direct max-bound guard

The candidate source was reverted because it did not clear the Score >= 2.0
keep gate.

## Baseline

Build:

```text
rch exec -- env CARGO_TARGET_DIR=target-icywolf-w7fbs-baseline cargo build --manifest-path artifacts/optimization/frankenredis-ds9o7/parser-harness/Cargo.toml --profile release-perf
```

Standalone baseline:

```text
MODE=borrowed-command FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000
178.2 ms +/- 17.1 ms
```

## Candidate

Focused parity gate passed before benchmark:

```text
rch exec -- env CARGO_TARGET_DIR=target-icywolf-w7fbs-test-focused cargo test -p fr-protocol parse_command_args_borrowed_into -- --nocapture
```

Result:

```text
4 focused borrowed parser tests passed
```

Standalone candidate:

```text
MODE=borrowed-command FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000
127.6 ms +/- 11.0 ms
```

The paired same-run comparison is the keep/reject decision:

```text
baseline  135.6 ms +/- 7.0 ms
candidate 131.7 ms +/- 5.0 ms
ratio       1.03x +/- 0.07
```

Score = Impact x Confidence / Effort = `1.03 x 0.80 / 1.00 = 0.82`.

This fails the Score >= 2.0 keep gate.

## Isomorphism

Golden outputs were byte-identical:

```text
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  baseline-golden-owned.canon
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  baseline-golden-borrowed.canon
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  candidate-golden-owned.canon
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  candidate-golden-borrowed.canon
```

Ordering, tie-breaking, floating point behavior, and RNG behavior were
unchanged; this parser path preserves byte order only and has no FP/RNG state.

## Next Primitive

The rejected lever says the bottleneck is no longer generic length parsing.
The next profile-backed primitive should attack the per-command allocation and
owned result shape in `parse_command_frame_borrowed`:

- inline small argv storage for common 1-4 argument commands, or
- runtime wiring to the already available caller-reused
  `parse_command_args_borrowed_into` buffer once `fr-runtime` is no longer
  reserved by active work.

Do not repeat length-parser micro-tuning.
