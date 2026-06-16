# Pass 223 proof - frankenredis-99fwc

Decision: rejected / evidence-only after rebasing over the peer `99fwc` and
`x1mmu` listpack-node work. The tested source lever was removed before this
amended commit; this artifact records why it did not clear the keep gate.

Target: profile-backed list DUMP gap from `frankenredis-99fwc`.

Lever tested: build RPUSH/promoted large lists directly as default-fill
quicklist-compatible listpack chunks, append new listpack entries incrementally,
and let DUMP/DEBUG trust those chunks only while the append-built
`list-max-listpack-size=-2` invariant is intact.

## Binaries

- Final measured baseline source: `4641482906a711c37cbef448486727f229922335`
- Current push parent: `d91e50721ce0e4e20e2a8577672bf8c058a56d02` (test-script only above the measured baseline)
- Rejected candidate source: `b3a8b72938a3d50f6c1b4a36d76737457a0269af`
- Baseline binary: `/data/projects/.scratch/frankenredis-coralox-pass223-final-baseline-target/release-perf/frankenredis`
- Baseline binary sha256: `597607d610f4c457be0cabe69b2c6fe88e3f13bb5b1bb55caa3cca68f94a8e30`
- Rejected candidate binary: `/data/projects/.scratch/frankenredis-coralox-pass223-final-candidate-target/release-perf/frankenredis`
- Rejected candidate binary sha256: `6168304d20a9cd3a999c9a7c6884a63127d4bca47c24c7fe651d29c4b0f40472`

`rch` remote execution was intentionally bypassed for this pass per the
2026-06-16 ts1-offline override. All builds and benches were local and
crate-scoped with `-j 1`.

## Benchmark

Final paired command:

```bash
hyperfine --warmup 1 --runs 7 --export-json artifacts/optimization/coralox-pass223-99fwc-listpack-build/paired-list-dump-hyperfine.json \
  'python3 artifacts/optimization/coralox-pass223-99fwc-listpack-build/list_dump_once.py --server-bin /data/projects/.scratch/frankenredis-coralox-pass223-final-baseline-target/release-perf/frankenredis --port 31301 --list-len 10000 --payload-size 16 --dumps 400 --dump-pipeline 16 --json-out artifacts/optimization/coralox-pass223-99fwc-listpack-build/baseline-list-dump-last.json --transcript-out artifacts/optimization/coralox-pass223-99fwc-listpack-build/baseline-list-dump-golden.resp' \
  'python3 artifacts/optimization/coralox-pass223-99fwc-listpack-build/list_dump_once.py --server-bin /data/projects/.scratch/frankenredis-coralox-pass223-final-candidate-target/release-perf/frankenredis --port 31302 --list-len 10000 --payload-size 16 --dumps 400 --dump-pipeline 16 --json-out artifacts/optimization/coralox-pass223-99fwc-listpack-build/candidate-list-dump-last.json --transcript-out artifacts/optimization/coralox-pass223-99fwc-listpack-build/candidate-list-dump-golden.resp'
```

Results:

- Hyperfine mean: `309.7 ms -> 341.1 ms`; baseline was `1.10x` faster within high candidate noise.
- Hyperfine median: `308.5 ms -> 319.6 ms`; candidate did not win wall time.
- Core DUMP loop: `560,702 ns/op -> 529,536 ns/op`, only `1.06x` faster after peer quicklist sealing was included.
- DUMP seconds for 400 operations: `0.224280830 -> 0.211814560`.
- DUMP payload bytes unchanged: `73568`.
- Load guardrail regressed: `0.009329523s -> 0.012051583s`.
- RSS delta guardrail: `960 KiB -> 904 KiB`.

Score: `Impact 0.5 * Confidence 3.0 / Effort 2.0 = 0.75`; below the `>=2.0`
keep gate. Source changes were removed.

## Golden proof

- DUMP digest sha256 unchanged: `4d7c09d7478db6f2d88d5960dba5b9fc1dc84e4c7066d84e093e63fd7a9715e2`.
- Normalized transcript sha256 unchanged: `fd59a87dd1072ba62410edb891bb2302b1db6a3a5525b38984144860ab955517`.
- `cmp -s baseline-list-dump-golden.resp candidate-list-dump-golden.resp` passed.

## Isomorphism notes

- Ordering: the rejected lever did not alter logical list order; final kept source has no code delta.
- Tie-breaking: no sorted/tie semantics participate.
- Floating point: no floating-point path participates.
- RNG: no RNG path participates.
- Final source state: behavior is identical to current main because all source hunks from the rejected candidate were removed.

## Gates run before rejection decision

- `cargo fmt -p fr-store -p fr-persist -- --check`
- `env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass223-rebase-check-target cargo check -j 1 -p fr-store -p fr-persist --all-targets`
- `env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass223-rebase-check-target cargo clippy -j 1 -p fr-store -p fr-persist --all-targets -- -D warnings`
- `env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass223-rebase-check-target cargo test -j 1 -p fr-store list -- --nocapture`
- `env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass223-rebase-check-target cargo test -j 1 -p fr-runtime dump_restore -- --nocapture`
- Local release-perf baseline and candidate `fr-server` builds passed.
