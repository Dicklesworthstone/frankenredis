# Pass 82: fixed small borrowed argv packet rejected

Bead: `frankenredis-6kecb`

## Profile basis

The fresh pass82 SETEX/PSETEX P16/1M server-only profile still showed the
generic parser-to-dispatch path after the command-specific SETEX/PSETEX,
static OK, matcher-router, runtime-info, client-tracking hash, and command-key
micro-levers were rejected. Relevant rows included
`parse_command_args_borrowed_into`, `process_buffered_frames`,
`Runtime::execute_frame_internal`, command metadata, and hashing costs.

## Lever tested

Candidate source diff: `candidate-source.diff`.

The candidate added a fixed-small borrowed argv packet in `fr-protocol` and fed
it through the existing `fr-server` borrowed fast-path matcher/generic fallback
logic. For commands with <= 8 bulk arguments, including SETEX/PSETEX, this
avoided the heap-backed `Vec<&[u8]>` parser buffer before canonical dispatch.
The existing Vec parser remained the fallback for larger arities.

The candidate did not change command implementations, dispatch ordering,
invalid-frame rejection, wrong-arity/unknown precedence, ACL/pubsub/transaction
gates, expiry math, propagation bytes, tie-breaking, floating-point behavior, or
RNG behavior.

## Validation while candidate was applied

- Baseline release-perf build via RCH:
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass82-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench`
- Candidate check via RCH passed:
  `cargo check -p fr-protocol -p fr-server --all-targets`
- Focused candidate tests via RCH passed:
  `cargo test -p fr-protocol try_parse_command_args_borrowed_small -- --nocapture`
  (3 tests)
- Focused server tests via RCH passed:
  `cargo test -p fr-server process_buffered_frames -- --nocapture`
  (2 tests)
- Candidate release-perf build via RCH passed:
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass82-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`
- `cargo fmt -p fr-protocol -- --check` and `cargo fmt -p fr-server -- --check`
  still report pre-existing unrelated rustfmt drift outside this candidate.
  The candidate source hunk was removed before this evidence commit.

Binary hashes:

- Baseline `frankenredis`: `06c0178e19350d8f6c52895ff4ba839c2fb55766f95fd8902ff93953a9f886a2`
- Baseline `fr-bench`: `a86e24c9f4baf7291fe72430fcc000b596e12d835e9f5759c14bd50b79b57f17`
- Candidate `frankenredis`: `3375cda5f36df841db8a012d47e45f57d812a8157609fd128880be395a235683`
- Candidate `fr-bench`: `4ab70546606676ef631dd72c896d88e292d8b6186672a80210af9af1609f3e72`

## Golden output

Comparator:

```bash
python3 artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py 27231 27232 artifacts/optimization/frankenredis-6kecb/pass82-small-argv-packet/golden-compare.json
```

Result:

- Baseline bytes: `992`
- Candidate bytes: `992`
- Baseline SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Candidate SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Equal: `true`

## Benchmark

Fresh pass82 one-sided baseline before the candidate, SETEX/PSETEX alternate
P16/1M, 50 clients, keyspace 10000, datasize 3:

- Baseline: `4.542s +/- 0.064s`

Paired hyperfine, baseline first:

- Baseline: `4.515587235482857s +/- 0.6206294871773717s`
- Candidate: `4.378462924197144s +/- 0.04384091749868783s`
- Summary: candidate `1.03x +/- 0.14x` faster, too noisy for a keep.

Reversed hyperfine, candidate first:

- Candidate: `4.3925447835s +/- 0.02924829486091379s`
- Baseline: `4.4538778735s +/- 0.030173209248694634s`
- Summary: candidate `1.01x +/- 0.01x` faster.

## Decision

Rejected under the Score>=2.0 keep gate. The candidate is byte-identical on the
SETEX/PSETEX golden transcript and passes focused checks, but the confirmed
speedup is only about `1.01x`, below the minimum impact/confidence threshold.
The production source hunk and candidate-only tests were removed.

Next route: stop fixed-small parser packet variants for this bead. Attack a
larger batched parser-to-dispatch metadata packet or event-loop batch primitive
that eliminates repeated command metadata/propagation work across a whole
pipeline window, target at least `1.20x` on SETEX/PSETEX P16/1M before any keep.
