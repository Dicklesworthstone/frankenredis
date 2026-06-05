# frankenredis-ds9o7 Proof

## Lever

Add a safe borrowed RESP command-argv parser in `fr-protocol`:

- `parse_command_frame_borrowed` borrows normal multibulk command arguments as
  `&[u8]` slices.
- `parse_command_args_borrowed_into` parses strict multibulk commands into a
  caller-reused argv buffer for future runtime hot-path wiring.
- Non-multibulk command-frame input falls back to the existing owned parser.

This pass is intentionally limited to the parser API, harness, and proof
artifacts. Runtime/server integration is a separate follow-up because peer work
currently owns those surfaces.

## Profile Target

Baseline harness:

```text
MODE=parse FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000
target-icywolf-ds9o7-baseline/release-perf/fr-protocol-parser-harness
```

Baseline hyperfine:

```text
371.7 ms +/- 10.5 ms
```

Baseline profile top self-costs:

```text
parse_bulk                 24.85%
cfree                      16.71%
parse_array                15.49%
parse_frame_internal       14.44%
drop_glue<RespFrame>       13.41%
```

The target was allocation-heavy owned bulk-string command parsing.

## Before/After

Final paired harness:

```text
MODE=owned-command FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000
MODE=borrowed-command FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000
target-icywolf-ds9o7-harness3/release-perf/fr-ds9o7-parser-harness
```

Hyperfine:

```text
owned-command     424.4 ms +/- 29.5 ms
borrowed-command  140.8 ms +/- 14.5 ms
ratio              3.01x faster
```

Borrowed profile top self-costs:

```text
parse_command_frame_borrowed 73.96%
harness main                 10.59%
RawVec allocation             6.04%
```

The old `parse_bulk` and `RespFrame` drop costs are removed from the top profile.
The remaining allocation cost is the per-call argv `Vec` in
`parse_command_frame_borrowed`; the caller-reused
`parse_command_args_borrowed_into` primitive exists for the next wiring pass.

## Isomorphism

- Ordering: preserved by pushing slices in RESP array order.
- Tie-breaking: not applicable; this parser does not sort or rank values.
- Floating point: not applicable; bytes are borrowed exactly as received.
- RNG: unchanged; no random state is read or written.
- Null/empty multibulk: `*-1` and `*0` have dedicated tests.
- Error semantics: tests compare owned and borrowed errors for non-bulk command
  arguments, null bulk command arguments, invalid lengths, incomplete payloads,
  and configured array/bulk size limits.

Golden canonical argv outputs:

```text
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  golden-owned-final.canon
468764ce7e7bcc2d2bcf29672fe3f5a2848eac2fa5972f769c58f8def1916d56  golden-borrowed-final.canon
```

`sha256sum -c artifacts/optimization/frankenredis-ds9o7/golden-final.sha256`
passes.

## Score Gate

```text
Impact     3.01
Confidence 0.95
Effort     0.60
Score      4.77
```

Score = Impact x Confidence / Effort. This clears the required 2.0 gate.

## Validation

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-icywolf-ds9o7-check2 cargo check -p fr-protocol --all-targets
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-icywolf-ds9o7-testfix cargo test -p fr-protocol parse_command_args_borrowed_into_reuses_buffer_and_clears_on_error -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-icywolf-ds9o7-testfull4 cargo test -p fr-protocol -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-icywolf-ds9o7-clippy4 cargo clippy -p fr-protocol --all-targets -- -D warnings
cargo fmt -p fr-protocol -- --check
cargo fmt --manifest-path artifacts/optimization/frankenredis-ds9o7/parser-harness/Cargo.toml -- --check
sha256sum -c artifacts/optimization/frankenredis-ds9o7/golden-final.sha256
ubs crates/fr-protocol/src/lib.rs artifacts/optimization/frankenredis-ds9o7/parser-harness/src/main.rs
```

Final gate results:

```text
fr-protocol tests: 76 unit, 2 fuzz corpus, 41 golden, 2 live oracle, doctests OK
clippy: OK with -D warnings
fmt: OK
golden sha256: OK
UBS: exit 0, 0 critical findings
```
