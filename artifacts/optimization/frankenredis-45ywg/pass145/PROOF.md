# frankenredis-45ywg pass145 proof

## Target

- Bead: `frankenredis-45ywg`
- Lever: cache parsed/resolved Lua chunks in a bounded thread-local cache and execute the cached immutable AST with a fresh `LuaState`, `Env`, KEYS, and ARGV per call.
- Profile backing: baseline trivial EVAL profile showed repeated parser/setup costs in `Lexer::tokenize_all_with_lines`, parser frames, `LuaState::new`, and table setup alongside syscall overhead.

## Binaries

- Baseline commit: `b456ecd721a95f72796ab154170ab68feb6d1d48`
- Baseline binary: `/data/projects/.scratch/frankenredis-45ywg-pass145-baseline-target/release-perf/frankenredis`
- Baseline sha256: `c1d9c9abfbd9eecabf78209fc242ee142cc2dea269ba2cc9f0393c3e2e6bf8df`
- Candidate binary: `/data/projects/.scratch/frankenredis-45ywg-pass145-candidate-target/release-perf/frankenredis`
- Candidate sha256: `56f1e609ed0f0d71216dba635c94d8dd3bad4a54f0805fdfede18a70fb42f7e5`

## Performance

Hyperfine was run against RCH-built release-perf binaries with the existing `lua_eval_bench.py` harness.

| Workload | Baseline | Candidate | Ratio |
| --- | ---: | ---: | ---: |
| trivial 20k paired | 1.497035182 s | 1.412030834 s | 1.0602x |
| trivial 20k reversed | 1.599366482 s | 1.479810248 s | 1.0808x |
| trivial 50k confirm | 3.812962695 s | 3.516328143 s | 1.0844x |
| table200 2500 paired | 0.677090341 s | 0.626395128 s | 1.0809x |
| loop1000 2500 guardrail | 0.987614812 s | 0.971889700 s | 1.0162x |

The loop1000 row is intentionally treated as a neutral guardrail because script execution dominates parse overhead there. The primary trivial row and table-construction row stayed directionally positive.

Score: Impact 3 x Confidence 3 / Effort 2 = 4.5. Keep threshold is 2.0.

## Isomorphism

- The cache key is the exact script source after the same shebang line-preserving whitespace normalization used by EVAL/SCRIPT LOAD.
- The cached value is an immutable parsed `Block` after local-slot resolution. Execution does not mutate the AST.
- Every invocation still creates a fresh `LuaState`, fresh top-level `Env`, fresh local cells, fresh table allocations, and fresh KEYS/ARGV bindings.
- Store state, command dispatch order, script propagation mode, read-only mode, RNG behavior, RESP version reset, error-line stamping, and reply conversion are still performed per execution.
- Function identity and closures are produced at execution time from the AST, not cached as runtime values.
- Bounded cache eviction clears the whole compiled-chunk map at 256 entries; eviction changes only whether a future call reparses.

## Golden Outputs

All baseline and candidate golden JSON outputs have sha256:

`522a3ab10859dcc45592ec5a323f1bc0f40b81db1662124aacb7a6d3e7bed005`

This matches the pass145 baseline and the prior pass144 Lua harness golden.

## Gates

- `cargo fmt -p fr-command -- --check`: pass
- RCH `cargo check -j 1 -p fr-command --all-targets`: pass on `vmi1152480`
- RCH `cargo test -j 1 -p fr-command --lib lua_ -- --nocapture`: pass, 202 passed on `vmi1152480`
- RCH `cargo clippy -j 1 -p fr-command --all-targets -- -D warnings`: pass on `vmi1152480`
