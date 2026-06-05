# frankenredis-5ciha Rejection

## Lever Tested

`frankenredis-5ciha` tested a fused borrowed RESP command length-line parser:

- scan for CRLF and validate the length line in one pass
- preserve `read_line` incomplete-vs-invalid behavior
- preserve `LineTooLong`
- preserve `-1` null length handling
- preserve leading-zero rejection, i64 bound, and max array/bulk errors

The candidate source was reverted because it regressed the benchmark.

## Baseline

The baseline binary was the pre-change rch-built ds9o7 parser harness:

```text
env MODE=borrowed-command FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000 target-icywolf-w7fbs-baseline/release-perf/fr-ds9o7-parser-harness
```

## Candidate

Focused parity gate:

```text
rch exec -- env CARGO_TARGET_DIR=target-icywolf-5ciha-test-focused cargo test -p fr-protocol parse_command_args_borrowed_into -- --nocapture
```

Result:

```text
4 focused borrowed parser tests passed
```

## Benchmark

Paired hyperfine:

```text
baseline  153.6 ms +/- 16.6 ms
candidate 168.8 ms +/-  5.2 ms
```

The baseline ran `1.10x +/- 0.12` faster than the candidate. This is a clear
reject, not a keep.

Score = Impact x Confidence / Effort = `0.91 x 0.90 / 1.00 = 0.82`.

## Isomorphism

Golden outputs were byte-identical:

```text
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  baseline-golden-borrowed.canon
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  baseline-golden-owned.canon
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  candidate-golden-borrowed.canon
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  candidate-golden-owned.canon
```

Ordering, tie-breaking, floating point behavior, and RNG behavior were
unchanged; this parser path preserves byte order only and has no FP/RNG state.

## Next Primitive

Do not continue tuning command length-line parsing. Two independent parser-line
micro-levers failed to clear the gate. The next profile-backed parser primitive
should attack result-shape allocation instead:

- inline small argv storage for common 1-4 argument commands, or
- runtime wiring to caller-reused `parse_command_args_borrowed_into` once the
  `fr-runtime` surface is no longer actively owned.
