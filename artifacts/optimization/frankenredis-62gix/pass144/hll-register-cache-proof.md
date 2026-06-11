# frankenredis-62gix pass144 proof

## Target

- Bead: `frankenredis-62gix`
- Hotspot: multi-key `PFCOUNT` repeatedly decoded the same serialized HyperLogLog strings into temporary register vectors.
- Lever: store a non-serialized register-resident HLL sidecar keyed by DB key and `Entry::modification_count`; multi-key `PFCOUNT` reuses it only when the current entry generation matches.

## Baseline

- Source: `a8eb47efa`
- Build: `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=target-62gix-baseline CARGO_BUILD_JOBS=1 cargo build -j 1 -p fr-server --profile release-perf`
- RCH worker: `vmi1152480`
- Hyperfine target workload: `PFCOUNT hllA hllB`, two 50k-element HLLs, 20k measured ops per harness run.
- Baseline hyperfine multi mean: `2.253468483945 s`
- Baseline internal loop: `9733.674219 ops/s`
- Baseline guard single-key mean: `1.595896851570 s`

## Candidate

- Build: `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=target-62gix-candidate CARGO_BUILD_JOBS=1 cargo build -j 1 -p fr-server --profile release-perf`
- RCH worker: `vmi1152480`
- Candidate hyperfine multi mean: `1.871917184785 s`
- Candidate internal loop: `11409.412574 ops/s`
- Candidate guard single-key mean: `1.576154718160 s`

## Result

- Hyperfine multi-key speedup: `1.203829x`
- Internal loop speedup: `1.172159x`
- Score: `Impact 1.203829 x Confidence 0.96 / Effort 0.55 = 2.10`
- Decision: keep. The win is modest but clears the `>=2.0` score gate with matched behavior proof, same RCH build worker, same binary profile, and a narrow safe-Rust sidecar lever.

## Isomorphism

- Observable bytes are unchanged: the cache is not part of `Value`, RDB/AOF/DUMP, `OBJECT ENCODING`, or HLL serialized payloads.
- Ordering is unchanged: multi-key `PFCOUNT` still iterates keys in caller order and merges registers with the same max operation.
- Tie-breaking is unchanged: HLL cardinality estimation reads the same merged register array and uses the existing estimator.
- Floating point is unchanged: no estimator arithmetic or score arithmetic changed.
- RNG is unchanged: the LFU `next_rand()` sampling condition remains before the entry mutation path, matching the previous per-key order.
- Mutation invalidation is preserved: full-key insert/remove/flush drops cache entries; in-place string changes are rejected by `Entry::modification_count` mismatch before any cached registers can be reused.

## Golden Output

- Baseline behavior SHA256: `c619bac67d64829d28058a8f77f6b2ef1a46167f5cca849fdda169b10c4a3dcf`
- Candidate behavior SHA256: `c619bac67d64829d28058a8f77f6b2ef1a46167f5cca849fdda169b10c4a3dcf`
- Transcript covers: `PFADD`, single and multi `PFCOUNT`, repeated multi `PFCOUNT`, `OBJECT ENCODING`, `PFDEBUG ENCODING`, `DUMP`, `PFMERGE`, and `SET` overwrite followed by multi-key `PFCOUNT` error preservation.

## Validation

- RCH `cargo test -j 1 -p fr-store pfcount_multi_key_register_cache -- --nocapture`: passed on `vmi1149989`.
- RCH `cargo check -j 1 -p fr-store --all-targets`: passed on `vmi1149989`.
- RCH `cargo clippy -j 1 -p fr-store --all-targets -- -D warnings`: passed on `vmi1152480`.
- `cargo fmt -p fr-store --check`: passed.
- `git diff --check`: passed.
- `ubs crates/fr-store/src/lib.rs crates/fr-store/src/packed_set.rs`: exit `1`; historical inventory remains (`218 critical`, `5674 warning`, `942 info`) and log saved to `ubs.log`.
