# frankenredis-cms7p Report

## Scope

- Bead: `frankenredis-cms7p`
- Target: LPUSH/RPUSH list-push hot path after pass169 attributed active LPUSH samples to `Store::drop_if_expired`.
- Lever: skip the lazy-expiry key lookup in `Store::lpush` and `Store::rpush` when `Store::expires_count == 0`.
- Decision: keep. Score `2.55` = Impact `3.0` x Confidence `0.85` / Effort `1.0`.

## Behavior Proof

- The guarded path is taken only when there are no whole-key expiries in the store. TTL-bearing keys still take the existing `drop_if_expired` path.
- Missing key, wrongtype, list order, LFU/RNG, dirty counter, and reply ordering semantics remain decided by the existing post-guard logic.
- Floating-point and tie-breaking behavior are not touched.
- Added `list_push_reaps_expired_key_before_mutation`, proving expired LPUSH/RPUSH keys are lazily deleted and recorded in `lazy_expired_propagation` before the new list mutation.
- Raw TCP golden covered persistent LPUSH/RPUSH ordering, WRONGTYPE, and expired-key LPUSH rewrite.
- Golden input SHA256: `e8eae7df83dcc3aaf85047b91c48b077099bc27976b80cdb817ad16faae6a15d`.
- Baseline and candidate output SHA256 both: `0ae52cb40e2687096669173b4bf060219b9079b66a7be54f61ac963c711820e8`.

## Benchmarks

- Baseline build: `rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-cms7p-baseline-target cargo build --release -p fr-server -p fr-bench`.
- Baseline LPUSH P16/C50/n1M fresh-state hyperfine: `1.472s +/- 0.077s`; last report `697001.32 ops/sec`, p99 `3573us`.
- Candidate build: `rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-cms7p-candidate-target cargo build --release -p fr-server -p fr-bench`.
- Candidate independent LPUSH P16/C50/n1M: `1.492s +/- 0.076s`; noisy and not used alone for keep.
- Paired LPUSH P16/C50/n1M, both servers live and flushed before each sample: candidate `1.339s +/- 0.065s`, baseline `1.460s +/- 0.132s`; candidate `1.09x +/- 0.11`.
- Long paired LPUSH P16/C50/n5M, candidate listed first to check order bias: candidate `6.667s +/- 0.305s`, baseline `7.586s +/- 0.123s`; candidate `1.14x +/- 0.06`.
- Last n5M throughput: candidate `773924.80 ops/sec`, baseline `655535.51 ops/sec`.
- Invalid artifact note: `paired-run2-n5m-candidate-first` contains a failed baseline server launch typo and is not evidence.

## Gates

- `rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-cms7p-test-lib-target cargo test -p fr-store --lib list_push_reaps_expired_key_before_mutation -- --nocapture` passed.
- `rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-cms7p-check-store-target cargo check -p fr-store --lib` passed.
- `rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-cms7p-candidate-target cargo build --release -p fr-server -p fr-bench` passed.
- `cargo fmt -p fr-store -- --check` failed on pre-existing broad formatting drift in `crates/fr-store/src/lib.rs` and `packed_set.rs`.
- `rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-cms7p-clippy-store-target cargo clippy -p fr-store --lib -- -D warnings` failed on pre-existing `collapsible_if` findings at `crates/fr-store/src/lib.rs:1382` and `:1653`.
- `ubs crates/fr-store/src/lib.rs` remained nonzero on pre-existing broad file-wide findings; no finding pointed at the guarded `lpush`/`rpush` hunk.

## Next Route

- Re-profile after this keep. The removed no-expiry lazy-expiry lookup should shift remaining LPUSH cost toward list value allocation/copy, per-command memory accounting, or socket pacing.
- Do not repeat exact parser scratch reuse or output coalescing without fresh profile proof.
