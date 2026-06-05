# frankenredis-2nhjg Rejection Proof

## Target

`parse_command_args_borrowed_into` in `crates/fr-protocol/src/lib.rs`.

One lever tested: replace command multibulk and bulk length parsing with a fused
strict ASCII decimal scanner instead of `read_line` plus `parse_i64_strict`.

## Baseline

Baseline harness:

```text
artifacts/optimization/icywolf-perf-20260605-pass48-protocol-decimal/bench_protocol_parse_baseline 2000000 16 64
```

Baseline hyperfine:

```text
mean = 466.1 ms +/- 11.5 ms
```

Baseline profile:

```text
98.85% fr_protocol::parse_command_args_borrowed_into
```

Existing equivalent parser-family rejection artifacts:

```text
frankenredis-w7fbs: 136.5 ms +/- 5.6 ms baseline vs 149.8 ms +/- 5.5 ms candidate
frankenredis-vng9a: 158.2 ms +/- 40.4 ms baseline vs 167.0 ms +/- 5.0 ms candidate
```

## Candidate Result

Candidate hyperfine:

```text
mean = 653.1 ms +/- 26.5 ms
```

The candidate was 1.40x slower than the baseline, so it fails the Score >= 2.0
keep gate.

Subagent paired benchmark for the same command length-scanner family also
regressed:

```text
before    = 123.1 ms +/- 4.4 ms
candidate = 151.3 ms +/- 10.1 ms
```

The old path was 1.23x faster there as well. Production code was reverted.

## Behavior Proof

Golden output check:

```text
sha256sum -c artifacts/optimization/frankenredis-ds9o7/golden-final.sha256
artifacts/optimization/frankenredis-ds9o7/golden-owned-final.canon: OK
artifacts/optimization/frankenredis-ds9o7/golden-borrowed-final.canon: OK
```

The pass-local `golden_before.sha256` and `golden_after.sha256` files are
byte-identical.

Ordering, tie-breaking, floating-point, and RNG are unchanged. This lever only
touched command length parsing during the rejected candidate run, and no
production code was kept.

## Next Primitive

No parser-only production lever remains for this profile slice. The next
profile-backed primitive is `frankenredis-lkj3q`: wire caller-reused
`parse_command_args_borrowed_into` storage into the runtime hot path. That bead
is currently blocked by `frankenredis-yaxr7.4`, owned by CrimsonHill.
