# Pass 39 Proof - Inline Integer String Values

## Target

- Bead: `frankenredis-20fi3`
- Profile-backed hotspot: integer string values in `fr-store` were stored as `Value::String(Vec<u8>)`, so the `INCRBY` counter loop allocated and inserted a fresh value `Vec` every update.
- Harness: `artifacts/optimization/blackthrush-perf-20260603-pass39-inline-int-values/harness`
- Baseline build: `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass39-inline-int-baseline-rch rch exec -- cargo build --manifest-path artifacts/optimization/blackthrush-perf-20260603-pass39-inline-int-values/harness/Cargo.toml --release`
- Candidate build: `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass39-inline-int-candidate-rch rch exec -- cargo build --manifest-path artifacts/optimization/blackthrush-perf-20260603-pass39-inline-int-values/harness/Cargo.toml --release`

## Lever

Add `Value::Integer(i64)` for canonical integer strings. `SET`, `SETNX`, `GETSET`, `INCR`, and `INCRBY` store canonical integer payloads inline. String read paths convert to the same decimal bytes that the previous `Vec<u8>` value held. String mutation paths materialize the integer into a `Vec<u8>` before mutation and preserve the previous raw-encoding transitions.

## Benchmark

- Baseline direct 1M: `0.152636045s`, `6551532.438 ops/sec`
- Candidate direct 1M: `0.121276285s`, `8245635.163 ops/sec`
- Paired hyperfine 10M baseline: `1.450s +/- 0.030s`
- Paired hyperfine 10M candidate: `1.209s +/- 0.043s`
- Hyperfine delta: `1.20x +/- 0.05x` faster
- Candidate `/usr/bin/time -v`: user time `0.13s`, max RSS `1664 KB`, minor faults `100`
- Candidate `strace -c`: `73` total syscalls, startup-only; no per-iteration I/O was introduced.
- Hardware `perf stat`: blocked by host `perf_event_paranoid=4` locally and through rch; see `candidate-perf-stderr.txt` and `candidate-perf-rch-stderr.txt`.

## Golden

Baseline and candidate golden transcripts match byte-for-byte.

```text
efd2f0d222b340ce440d36eafaf86af53ab2023c943a636c597ea30645ed729e
```

The transcript covers seed `GET`/`OBJECT ENCODING`/`MEMORY USAGE`, `INCR`, post-`INCR` read/encoding, `INCRBY -3`, post-negative read/encoding, `STRLEN`, and final memory usage.

## Isomorphism

- Ordering and tie-breaking: `IndexMap`/key ordering and iteration paths are unchanged. `Integer` serializes into the same bytes as the prior stored string for digest, AOF, RDB, DUMP, and read paths.
- Integer spelling: canonical detection reuses `parse_i64`, preserving existing treatment for `-0`, leading-zero strings, overflow, non-UTF8 bytes, and non-integers as raw strings.
- String semantics: `GET`, `MGET`, `GETRANGE`, `GETBIT`, `BITCOUNT`, `BITPOS`, `BITFIELD GET`, `STRLEN`, `TYPE`, `OBJECT ENCODING`, and `MEMORY USAGE` observe integer values as Redis string objects.
- Mutations: `APPEND`, `SETRANGE`, `SETBIT`, and `BITFIELD SET` materialize decimal bytes before mutation and keep raw encoding, matching the old post-mutation object state.
- HLL: integer-encoded strings feed their decimal bytes into HLL validation, so invalid-HLL behavior is preserved instead of returning generic wrong-type.
- Floating point: `INCRBYFLOAT` still stores string bytes and keeps `force_string_encoding`; no floating-point arithmetic or formatting path was changed.
- RNG/LFU: LFU random sampling remains at the same lookup sites; the representation change does not add or remove RNG calls on covered paths.

## Validation

- `cargo fmt -p fr-store -p fr-command -p fr-runtime --check`
- `rch exec -- cargo test -p fr-store integer_string_values_keep_string_semantics_without_vec_payload -- --nocapture`
- `rch exec -- cargo test -p fr-store hll_commands_validate_integer_encoded_strings_as_string_bytes -- --nocapture`
- `rch exec -- cargo check -p fr-store --all-targets`
- `rch exec -- cargo check -p fr-runtime --all-targets`
- `rch exec -- cargo check -p fr-conformance --all-targets`
- `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`
- `rch exec -- cargo clippy -p fr-command --all-targets -- -D warnings`
- `rch exec -- cargo clippy -p fr-runtime --all-targets -- -D warnings`
- `ubs crates/fr-store/src/lib.rs crates/fr-command/src/lib.rs crates/fr-runtime/src/lib.rs` was attempted; the UBS Rust scanner hung for several minutes in `ubs-rust.sh` / `ast-grep` and was terminated as a tooling timeout.

## Score

Impact `3.0` x Confidence `0.9` / Effort `1.0` = `2.7`.

Verdict: keep.
