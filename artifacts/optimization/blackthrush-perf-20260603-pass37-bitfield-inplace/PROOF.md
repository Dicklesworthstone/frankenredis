# Pass 37 Proof: BITFIELD SET In-Place Mutation

Bead: `frankenredis-gu5nf.26`

## Profile-Backed Target

`Store::bitfield_set` cloned and reinserted the entire string value for every `BITFIELD SET` write. On a 32 MiB string with 128 one-byte writes, the baseline harness spent almost all traced syscall time releasing large allocations:

- Direct baseline: `2.757758070s`, `46.415 ops/s`
- Hyperfine baseline: `2.820459498785s +/- 0.0482453511285s`
- Strace baseline: `munmap` dominated traced time

## One Lever

Existing string entries are now mutated in place after reading the old bitfield value. Missing keys still allocate a new string. Wrong-type, max-size, raw-encoding, dirty, digest-stale, LFU/LRU timestamp, modification-count, TTL-count, and expiry behavior are preserved.

## Benchmark

- Candidate direct: `0.000010480s`, `12213740.458 ops/s`
- Candidate hyperfine: `0.03201189013s +/- 0.001516210575s`
- Hyperfine speedup: `88.11x`
- Score: Impact `5.0` x Confidence `0.95` / Effort `1.0` = `4.75`

## Isomorphism Proof

- Ordering preserved: yes; one key is mutated in place and no iteration order is changed.
- Tie-breaking unchanged: yes; bit offsets, bit widths, old-value replies, and missing-key creation follow the same read/write helpers.
- Floating-point: N/A.
- RNG: N/A.
- Golden output: `sha256sum -c golden-output.sha256` passed.
- Golden sha256: `5275bc4bba44e25966b62c11c46be73554387f08dbcf6e91cc2cc5e95a7684d2`

## Validation

- `cargo fmt -p fr-store --check`: passed.
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass37-bitfield-test-final-rch rch exec -- cargo test --profile release-perf -p fr-store bitfield_set_large_string_in_place_matches_clone_reference_and_reports_ab_ratio -- --nocapture`: passed.
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass37-bitfield-check-rch rch exec -- cargo check -p fr-store --all-targets`: passed.
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass37-bitfield-clippy-nodeps-rch2 rch exec -- cargo clippy -p fr-store --all-targets --no-deps -- -D warnings`: passed.
- Full dependency clippy for `fr-store` is blocked by an unrelated `fr-persist` `clippy::never_loop` lint.
- Optional `RCH_FORCE_REMOTE=true ... rch exec -- cargo test --profile release-perf -p fr-store bitfield -- --nocapture` hit an `rch` sync fallback after a transient `temp-*.rdb` vanished, then completed locally: 3 matching tests passed.
- `ubs crates/fr-store/src/lib.rs`: completed with the repo's existing broad `fr-store` inventory; UBS internal fmt/clippy/check/test-build sections were clean.
