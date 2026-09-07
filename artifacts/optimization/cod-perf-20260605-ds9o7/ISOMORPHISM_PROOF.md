# frankenredis-ds9o7 Isomorphism Proof

## Scope

This pass adds a safe borrowed RESP multibulk command argv parser in
`fr-protocol`:

- `parse_command_frame_borrowed`
- `parse_command_args_borrowed_into`

The measured hot-path primitive borrows each bulk argument as `&[u8]` and
reuses a caller-owned argv buffer. It does not wire the server/runtime command
path yet; that remains a separate follow-up lever so this commit keeps exactly
one optimization surface.

## Profile Target

The bead target was profile-backed on the command parser harness:

- `fr_protocol::parse_bulk`: 25.56 percent
- `cfree`: 17.23 percent
- `fr_protocol::parse_array`: 16.56 percent
- `drop_glue<RespFrame>`: 11.21 percent
- `RawVec` allocation: 7.32 percent
- `malloc`: 5.21 percent

The primitive removes the per-argument `Vec<u8>` allocation/copy and avoids
owned `RespFrame` construction for strict multibulk commands.

## Behavioral Isomorphism

- Ordering: borrowed argv slices are pushed in the exact RESP array order.
- Cursor semantics: tests assert borrowed `consumed` matches
  `parse_command_frame` and leaves trailing bytes untouched.
- Error semantics: table tests compare borrowed errors against
  `parse_command_frame` for non-bulk elements, null bulk args, malformed
  multibulk lengths, malformed bulk lengths, incomplete bodies, invalid
  terminators, and RESP3 prefixes with `allow_resp3` false and true.
- Limits: tests assert `max_bulk_len` and `max_array_len` boundaries preserve
  the owned parser's outcomes.
- Null and empty arrays: `*-1` remains distinguishable from `*0`.
- Binary payloads: tests cover empty args and binary args with embedded CRLF
  inside the declared bulk body.
- Tie-breaking: not applicable; parser preserves byte order and cursor
  position only.
- Floating point: not applicable; no numeric computation changed.
- RNG: not applicable; no randomized behavior changed.

## Golden Output

The harness writes canonical `len:arg` lines from both the owned parser and the
borrowed parser, asserts byte equality, then emits the canonical output.

Golden SHA256:

```text
0cd5dd5aa7f5872edd69e51e9f6e4dfb0987a6142f600c96afcb6b56992cdd3d  artifacts/optimization/cod-perf-20260605-ds9o7/golden-owned-borrowed.txt
```

Verification:

```text
artifacts/optimization/cod-perf-20260605-ds9o7/golden-owned-borrowed.txt: OK
```

## Benchmarks

Workload:

```text
FIELDS=16 VALUE_SIZE=64 ITERATIONS=200000
```

Direct before/after:

```text
before owned generic baseline: 383.6 ms +/- 15.1 ms
after borrowed command argv:    88.8 ms +/-  3.5 ms
speedup:                       4.32x +/- 0.24
```

Candidate same-harness controls:

```text
owned-generic:     439.2 ms +/-  7.4 ms
owned-command:     431.2 ms +/- 72.3 ms
borrowed-command:   87.6 ms +/-  3.1 ms
same-harness gain: 4.92x over owned-command
```

## Quality Gates

```text
rch exec -- env CARGO_TARGET_DIR=target-ds9o7-test-borrowed-rch cargo test -p fr-protocol borrowed -- --nocapture
rch exec -- env CARGO_TARGET_DIR=target-ds9o7-test-protocol-rch cargo test -p fr-protocol -- --nocapture
rch exec -- env CARGO_TARGET_DIR=target-ds9o7-clippy-rch cargo clippy -p fr-protocol --all-targets -- -D warnings
rch exec -- env CARGO_TARGET_DIR=target-ds9o7-harness-clippy-rch cargo clippy --manifest-path artifacts/optimization/cod-perf-20260605-ds9o7/parser-harness/Cargo.toml -- -D warnings
cargo fmt -p fr-protocol -- --check
cargo fmt --manifest-path artifacts/optimization/cod-perf-20260605-ds9o7/parser-harness/Cargo.toml -- --check
ubs crates/fr-protocol/src/lib.rs artifacts/optimization/cod-perf-20260605-ds9o7/parser-harness/src/main.rs
```

Results:

- Focused borrowed tests: 7 passed.
- `fr-protocol` tests: 76 unit tests, 2 fuzz-corpus tests, 41 golden tests, 2
  live-oracle tests, doc tests all passed.
- Clippy: passed with `-D warnings`.
- Format checks: passed.
- UBS changed-file scan: passed with exit code 0.

## Score

Impact x Confidence / Effort = `4.32 x 4 / 2 = 8.64`.

The change clears the Score >= 2.0 keep gate.
