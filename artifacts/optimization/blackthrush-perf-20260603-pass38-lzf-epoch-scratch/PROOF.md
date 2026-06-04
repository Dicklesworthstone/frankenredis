# Pass 38 Proof: LZF Epoch Scratch

Bead: `frankenredis-gu5nf.27`
Agent: `BlackThrush`
Scope: one lever in `crates/fr-persist/src/lib.rs`

## Profile-Backed Target

The LZF compressor allocated and zeroed a 65,536-slot `u32` hash table on every call. The pass-38 harness compressed 20,000 deterministic 96-byte payloads and therefore logically rebuilt `5,242,880,000` bytes of scratch-table state. `perf stat` was blocked by host `perf_event_paranoid=4`, but `strace` showed no per-call syscall churn after startup, leaving the repeated CPU/memory-table rebuild as the profiler-evident hotspot.

Baseline evidence:

- Direct 20k: `0.047939107s`, `417195.923 ops/sec`.
- Hyperfine 20k: `69.1 ms +/- 4.7 ms`.
- Long direct 200k: `0.515383911s`, `388060.232 ops/sec`.
- `/usr/bin/time -v` baseline 20k: max RSS `1628 KB`, minor faults `158`.

## Lever

Replace the per-call zeroed hash table with a safe-Rust thread-local `LzfScratch` containing epoch-tagged hash slots. A slot is visible only when its `generation` matches the current call, so stale slots read exactly like zero-initialized entries. On `u32` generation wrap, the scratch table is explicitly cleared and generation restarts at `1`.

This is the alien-graveyard region/scratch-reuse primitive with generation tags: reuse the scratch region, reset logically by epoch, and avoid bulk clear on the hot path.

## Isomorphism Proof

- LZF hash lookup semantics are unchanged: old `0` means unset; new stale-generation slots also return `0`.
- Match search, hash formula, match-length loop, budget rejection, literal flushing, post-match rehash, and compressed byte emission are unchanged.
- `Some`/`None` decisions remain governed by the same budget checks and output lengths.
- RDB raw-vs-LZF choice remains byte-equivalent because `rdb_encode_string` still calls `lzf_compress` and receives the same bytes or `None`.
- Ordering, tie-breaking, floating point, and RNG behavior are unaffected; this path has no FP or RNG and does not change iteration order.
- Golden output is byte-identical:
  - baseline `9848ed7786f6efd97c94e7b571eb2652c1389aaa35d628309bbe9e157fb3b516`
  - final candidate `9848ed7786f6efd97c94e7b571eb2652c1389aaa35d628309bbe9e157fb3b516`
- Harness checksums also match:
  - direct 20k: `compressed=17143 total_out=702831 checksum=57184`
  - long 200k: `compressed=171429 total_out=7028557 checksum=571406`

## Final Benchmark

Final candidate direct evidence:

- Direct 20k: `0.005414574s`, `3693734.724 ops/sec`.
- Direct 200k: `0.037581815s`, `5321722.753 ops/sec`.
- `/usr/bin/time -v` candidate 20k: max RSS `2112 KB`, minor faults `222`.

Final paired hyperfine over the 200k workload:

- Baseline: `479.0 ms +/- 17.9 ms`.
- Candidate: `40.2 ms +/- 2.1 ms`.
- Speedup: `11.90x +/- 0.77x`.

The retained thread-local scratch costs roughly one 512 KiB table per thread, reflected in the RSS increase from `1628 KB` to `2112 KB`, and removes repeated 256 KiB per-call table rebuilds on the compression path.

## Validation

Passed:

- `cargo fmt -p fr-persist --check`
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass38-lzf-test-lzf-final-rch rch exec -- cargo test -p fr-persist lzf_compress -- --nocapture`
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass38-lzf-test-rdbstring-final-rch rch exec -- cargo test -p fr-persist rdb_encode_string -- --nocapture`
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass38-lzf-test-rdbdecode-rch rch exec -- cargo test -p fr-persist rdb_decodes_lzf_encoded_string_values -- --nocapture`
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass38-lzf-test-fuzzseeds-rch rch exec -- cargo test -p fr-persist encode_rdb_round_trip_invariants_for_fuzz_seeds -- --nocapture`
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass38-lzf-test-determinism-rch rch exec -- cargo test -p fr-persist mr_rdb_encoding_determinism -- --nocapture`
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass38-lzf-check-rch2 rch exec -- cargo check -p fr-persist --all-targets`
- `RCH_FORCE_REMOTE=true CARGO_TARGET_DIR=target-blackthrush-pass38-lzf-clippy-rch2 rch exec -- cargo clippy -p fr-persist --all-targets -- -D warnings`

UBS:

- `ubs crates/fr-persist/src/lib.rs` exited `1` with the existing broad `fr-persist` warning inventory; its internal fmt/clippy/check/test-build sections were clean. Full output is recorded in `ubs-final.txt`.

Coordination:

- Bead claim: `frankenredis-gu5nf.27` assigned to `BlackThrush`.
- Agent Mail reservation attempts failed because the mailbox activity lock was busy; Beads claim remained the durable coordination record.

## Score

Impact `5.0` x Confidence `0.95` / Effort `1.0` = `4.75`.

Verdict: KEEP. Close `frankenredis-gu5nf.27` and re-profile before the next optimization because the LZF scratch-table bottleneck has shifted.
