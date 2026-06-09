# Pass 107: lazy borrowed SET classification rejection

Bead: `frankenredis-o7big`

Parent: `frankenredis-zhphm`

## Profile target

Current pushed base: `d50332f1dfa62d76cf841f06b9012d4e9546c471`.

RCH build:

- `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`
- Worker: `vmi1227854`
- Baseline `frankenredis` SHA-256:
  `ac1ea57c9b57b17d2625750c2cac5d8a32d60142366e7c9cd3a4fbe7da6bb4d8`
- Baseline `fr-bench` SHA-256:
  `9cfab4f5df391c287ef2e9074c2c1ded21edb1fbefe6fadec71df263a8bdb6aa`

Current baseline:

- SET P16/300k hyperfine: `463.3 ms +/- 71.5 ms` (noisy).
- SET P16/1M profile run: `844213.73 ops/sec`, p50 `872us`, p95 `1306us`,
  p99 `1772us`, p999 `2895us`.
- Perf samples: 776 cycles samples, 0 lost.
- Top no-children rows:
  - `fr_store::canonical_string_value_from_slice`: `15.49%`
  - `fr_protocol::parse_command_args_borrowed_into`: `2.52%`
  - `[vdso]`: `2.42%`
  - `core::ptr::drop_in_place::<fr_store::Value>`: `2.08%`
  - `<fr_store::Store>::set_plain_borrowed`: `1.95%`

## Lever

Borrowed plain SET stopped eagerly calling `parse_i64` during storage. It
stored `Value::String(SmallStr::from_slice(value))` directly for both missing
and existing-key borrowed SET paths.

This was a structural lazy-classification attempt, not the earlier first-byte
parse predicate: Redis integer encoding was deferred to already-existing
observation/arithmetic boundaries (`OBJECT ENCODING`, `MEMORY USAGE`,
`OBJECT REFCOUNT`, `INCR`/`INCRBY`, DUMP/AOF/state digest), all of which derive
the same canonical integer semantics from bytes.

## Isomorphism proof

- Ordering: unchanged; parser, dispatch, key order, and single-thread execution
  order were untouched.
- Tie-breaking: unchanged; no ordered set/list/hash comparison path changed.
- Floating-point: untouched.
- RNG: untouched; LFU random sampling and RANDOMKEY state were not changed.
- Redis-visible bytes: GET, DUMP, COPY, OBJECT ENCODING/REFCOUNT, MEMORY USAGE,
  INCR, noncanonical integers, overflow integers, and nonnumeric strings were
  covered by focused tests and golden TCP output.
- Integer spelling boundary: existing `parse_i64` rules remained the
  arithmetic gate; noncanonical values such as `007`, `-0`, and overflow remain
  string values for integer commands.

Validation:

- `rustfmt --edition 2024 --check crates/fr-store/src/lib.rs`: passed.
- `rch exec -- cargo test -p fr-store set_plain_borrowed_matches_set_for_new_integer_and_string_values -- --nocapture`:
  passed on `vmi1227854`.
- `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`:
  passed on `ovh-a`.

Golden:

- Command transcript: 27 commands.
- Baseline output bytes: 284.
- Candidate output bytes: 284.
- SHA-256:
  `7d8b2be372ee1d89095e4c8e1864d5e62505812e8118e527a1a5296fb8df906b`
  for both baseline and candidate.

Candidate build hashes:

- Candidate `frankenredis` SHA-256:
  `7b538e39114a6fe10f0c2cc1e50b8cea0140a76f4d059c868264660b4f76c6c8`
- Candidate `fr-bench` SHA-256:
  `eb2321bda032b0f704669e2832f7d94aada2593b6ab7617f9d6e373e2315bccd`

## Benchmarks

SET P16/1M, 50 clients, pipeline 16, datasize 3. The same baseline
`fr-bench` binary drove both servers.

Paired:

- Baseline: `1.818 s +/- 0.061 s`
- Candidate: `1.825 s +/- 0.161 s`
- Hyperfine summary: baseline `1.00x +/- 0.09` faster

Reversed:

- Candidate: `1.283 s +/- 0.020 s`
- Baseline: `1.246 s +/- 0.027 s`
- Hyperfine summary: baseline `1.03x +/- 0.03` faster

## Decision

Reject under Score>=2.0.

Score estimate: Impact `0.0` x Confidence `4.0` / Effort `2.0` = `0.0`.

The source hunk was not shipped. Evidence is retained here plus the current
profile under
`artifacts/optimization/tealotter-pass107-current-reprofile-20260609/`.

## Next route

Stop SET integer-classification variants. The next `frankenredis-zhphm` pass
should attack a larger safe-Rust IO/command-packet primitive: batch-level
read/parse/write ownership or a fixed worker handoff that removes syscall and
metadata overhead as a class while keeping command execution serial.
