# Pass 190 Rejection Report - frankenredis-ohsk5.57

## Target

- Bead: `frankenredis-ohsk5.57`
- Profile row: child-owned GDB sample during INCR P16/C50 landed in `hashbrown::HashMap::remove` for `Store::hll_register_cache` at `crates/fr-store/src/lib.rs:5984`, called by `Store::incrby_existing_or_insert`.
- Local sampling blockers: `perf_event_paranoid=4` blocked `perf` and `samply`; direct GDB attach was blocked by ptrace policy. The sample was acquired by running the server as a GDB child.

## Baseline

- Source context: current working tree at HEAD `331491f35a4c16039a0fecdffd122de5f7dc393d`; unrelated peer edits were present and held constant between baseline and candidate.
- RCH build: `cargo build --profile release-perf -p fr-server -p fr-bench` with `CARGO_TARGET_DIR=/data/projects/frankenredis/target-coralox-pass190-baseline`, worker `vmi1153651`.
- Baseline binary SHA256:
  - `frankenredis`: `2aed924f05a4425f9e57ab2bb870959abd924dd74459b9348c83749a894e130c`
  - `fr-bench`: `8a82960b3348a8ab020a4e36e6c2f8e93846dd75ed8b3b333717a46968667b37`
- Baseline INCR P16/C50/n300k hyperfine: `363.4 ms +/- 11.5 ms`.
- Baseline raw RESP golden SHA256: `a0bd9857da9b33d168ae8e0755e2f7f258e2af14f5aa7cda3dccfa90b2f8d5e8`, 196 bytes.

## Candidate Tried

- One lever: skip `self.hll_register_cache.remove(key)` on successful numeric INCR/INCRBY writes.
- Proof argument while applied: INCR/INCRBY advances `Entry::modification_count`; multi-key PFCOUNT only uses an HLL register-cache entry when the cached modification count equals the entry modification count, so stale private cache entries are ignored and current bytes are reparsed.

## Behavior Proof While Applied

- Raw RESP golden transcript covered numeric INCR, GET, PFADD, multi-key PFCOUNT cache population, single-key PFCOUNT, invalid-HLL error behavior on integer data, minus-zero rejection, and integer overflow.
- Candidate raw RESP golden SHA256: `a0bd9857da9b33d168ae8e0755e2f7f258e2af14f5aa7cda3dccfa90b2f8d5e8`, 196 bytes.
- Baseline and candidate golden outputs were byte-identical.
- Ordering, tie-breaking, floating-point behavior, and RNG behavior are unchanged by this candidate; the touched path has no FP or RNG operations and preserves serial command execution.

## Validation While Applied

- `cargo fmt -p fr-store --check`: passed.
- RCH `cargo test -p fr-store incrby_existing_key_matches_entry_replacement_side_effects`: passed.
- RCH `cargo test -p fr-store pfcount_multi_key_register_cache`: passed two HLL cache tests.
- RCH `cargo check -p fr-store --all-targets`: passed.
- RCH `cargo clippy -p fr-store --all-targets -- -D warnings`: passed.
- RCH release-perf candidate build succeeded on `vmi1152480`.
- Candidate binary SHA256:
  - `frankenredis`: `802d309aa9d3c1f56a2ee38d1b6e30cc1bac57643fb2317eb2b68182ad8714c0`
  - `fr-bench`: `91f75bd63786bee7952097e859e7e3cc6380097a2438f5f2278658db74d9798f`

## Benchmark Result

- Candidate-only INCR P16/C50/n300k hyperfine: `601.3 ms +/- 108.9 ms`.
- Paired same-window hyperfine:
  - Baseline: `609.1 ms +/- 51.4 ms`
  - Candidate: `621.3 ms +/- 54.8 ms`
  - Baseline was `1.02x +/- 0.12` faster.
- Score: Impact `-0.2` x Confidence `2.0` / Effort `1.0` = negative. Does not meet Score >= 2.0.

## Decision

- Rejected. Production source hunk removed before closeout.
- Do not repeat INCR HLL-cache invalidation micro-levers without a new active profile row and a materially different primitive.

## Next Route

- Attack a deeper alien-graveyard primitive next: batch-scoped command packets with range-backed RESP argv storage plus owned output batching, selected from the safe-Rust arena/slab and data-plane batching families.
- Target ratio: at least `1.15x` on the current top P16/C50 residual with identical raw RESP golden SHA256 and preserved command ordering.
- First requirement: acquire a fresh userspace profile row in a perf-capable environment, or a child-owned GDB active-frame row that names parser argv materialization, command-packet metadata, arena/slab allocation, or output ownership as the measured hotspot.
