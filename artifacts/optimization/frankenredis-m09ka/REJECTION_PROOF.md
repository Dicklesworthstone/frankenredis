# frankenredis-m09ka rejection proof

## Target

- Bead: `frankenredis-m09ka`
- Lever tested: replace the per-command `Vec<&[u8]>` returned by
  `parse_command_frame_borrowed` with an inline borrowed argv result for the
  common 1-4 argument command shape.
- Profile-backed source: prior borrowed-command profiles still showed
  `RawVec` allocation after `frankenredis-ds9o7`; `frankenredis-w7fbs` and
  `frankenredis-vng9a` rejected length-line micro-levers, so this pass tested
  a different allocation-layout primitive.

## Baseline

Built with RCH:

```bash
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-icywolf-m09ka-baseline-harness \
  cargo build --manifest-path artifacts/optimization/frankenredis-ds9o7/parser-harness/Cargo.toml \
  --profile release-perf
```

Standalone baseline:

```text
MODE=borrowed-command FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000
mean = 133.5 ms +/- 4.0 ms
```

Artifact:
`artifacts/optimization/frankenredis-m09ka/baseline-borrowed-command-hyperfine.json`

## Candidate

Candidate built with RCH:

```bash
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-icywolf-m09ka-candidate-harness \
  cargo build --manifest-path artifacts/optimization/frankenredis-ds9o7/parser-harness/Cargo.toml \
  --profile release-perf
```

Targeted behavior test while candidate was applied:

```bash
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-icywolf-m09ka-test-targeted \
  cargo test -p fr-protocol parse_command_frame_borrowed -- --nocapture
```

Result: 3 borrowed-frame unit tests passed.

Canonical output proof while candidate was applied:

```text
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  golden-owned.canon
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  golden-borrowed.canon
```

Ordering is RESP array order. Tie-breaking, floating-point behavior, and RNG
state are not touched by the parser result-shape candidate.

## Paired benchmark

Command:

```bash
hyperfine --warmup 3 --runs 12 \
  --export-json artifacts/optimization/frankenredis-m09ka/paired-baseline-vs-candidate-hyperfine.json \
  'MODE=borrowed-command FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000 target-icywolf-m09ka-baseline-harness/release-perf/fr-ds9o7-parser-harness' \
  'MODE=borrowed-command FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000 target-icywolf-m09ka-candidate-harness/release-perf/fr-ds9o7-parser-harness'
```

Result:

```text
baseline  = 129.8 ms +/- 3.6 ms
candidate = 153.3 ms +/- 6.7 ms
baseline ran 1.18x faster than candidate
```

Clean `HEAD` rerun after the repo advanced to `ae6a38804` used a detached
scratch worktree at
`/data/projects/.scratch/frankenredis-m09ka-baseline-ae6a38804-icywolf` so the
baseline binary did not include candidate source changes.

Standalone clean baseline:

```text
baseline = 130.5 ms +/- 3.4 ms
```

Fresh paired rerun:

```text
baseline  = 133.5 ms +/- 6.4 ms
candidate = 161.3 ms +/- 9.7 ms
baseline ran 1.21x faster than candidate
```

Artifacts:

- `baseline-clean-head-borrowed-command-hyperfine.json`
- `baseline-clean-head-borrowed-command-hyperfine.txt`
- `paired-clean-head-vs-inline-small-hyperfine.json`
- `paired-clean-head-vs-inline-small-hyperfine.txt`
- `golden.sha256`
- `golden.sha256check.txt`

Score: rejected. Impact is below 1.0 because the candidate regressed; it fails
the required Score >= 2.0 gate.

## Decision

Do not ship inline returned argv storage. The allocation avoided by replacing
`Vec<&[u8]>` is outweighed by copying the larger inline result value through the
parse return path on the profiled command shape.

No source code from this lever is retained.

Next deeper primitive: wire the already-shipped caller-reused
`parse_command_args_borrowed_into` API into the runtime session path so argv
storage lives in reusable per-client state instead of being returned by value.
If the runtime surface is peer-owned, pivot to an event-loop batching or
zero-copy framing primitive with fresh profile evidence.
