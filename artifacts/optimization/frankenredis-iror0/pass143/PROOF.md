# Pass 143 Proof - frankenredis-iror0

## Target

Profile-backed bead `frankenredis-iror0`: Lua EVAL loops were 5-9x slower than
Redis. Prior diagnostics showed per-access local lookup dominated: each
variable read/write walked scope maps and hashed string names.

## Lever

Replace each Lua scope's `HashMap<String, Rc<RefCell<LuaValue>>>` with a compact
`Vec<LocalBinding>`.

- `set_local` preserves "latest local in current scope" semantics by replacing
  the most recent same-name binding.
- `get_local`, `set_existing_local`, and name classification preserve the old
  innermost-to-outermost lookup order.
- Captured upvalue cells remain `Rc<RefCell<LuaValue>>`; closure capture still
  clones cells, not values.
- Numeric `for` loop iterations still allocate a fresh loop variable cell per
  iteration, preserving Lua 5.1 closure capture behavior.

This is the first safe structural step toward full slot-resolved locals. It
removes HashMap hashing from hot local access but still scans names, so the
follow-up remains a parse-time slot resolver.

## Performance

Release-perf server binaries were built via RCH.

- Baseline binary: `/data/projects/.scratch/frankenredis-iror0-pass143-baseline-target/release-perf/frankenredis`
  - SHA-256: `c7a5dd309f295cfeba8a274d5fd2608139e7b896d2c4fc41fb45b78a38134bbf`
- Candidate binary: `/data/projects/.scratch/frankenredis-iror0-pass143-candidate-target/release-perf/frankenredis`
  - SHA-256: `0a9eddf667d170dd91a17341652307eed011086a109f7d190df40d9259c94391`

Benchmarks:

- `loop1000`, paired order: baseline `1.508s +/- 0.051s`, candidate
  `1.069s +/- 0.049s`, speedup `1.41x +/- 0.08`.
- `loop1000`, reversed order: candidate `1.085s +/- 0.034s`, baseline
  `1.485s +/- 0.074s`, speedup `1.37x +/- 0.08`.
- `table200`, paired order: baseline `963.7ms +/- 24.3ms`, candidate
  `754.9ms +/- 39.7ms`, speedup `1.28x +/- 0.07`.

Artifacts:

- `paired-loop1000-hyperfine.json` SHA-256:
  `2046a80e203f10ffb7a8dc1613e77235e65a8c0bdf31710ec1f98acef2bf5c41`
- `reversed-loop1000-hyperfine.json` SHA-256:
  `9f4591f4d045f7be2b42e64683bd5ad12d5ee03d54fd9b9e11dc37b90730b324`
- `paired-table200-hyperfine.json` SHA-256:
  `2c4c821dc87792058e299e8cc92796f72d242ca5b66ecfa5bafc816756d982a4`

Score: Impact `1.41` x Confidence `0.95` / Effort `0.60` = `2.23`; keep.

## Isomorphism Proof

Golden behavior transcript is byte-identical across baseline and candidate.

- Golden JSON SHA-256 for every baseline/candidate/reversed run:
  `522a3ab10859dcc45592ec5a323f1bc0f40b81db1662124aacb7a6d3e7bed005`
- Transcript SHA-256 inside the golden files:
  `6e20e28314978053709dee6ae7958ababe6c7c76b73e7e136152696cabceda08`

Transcript cases:

- `return 1` -> `:1`
- `local x=0 for i=1,1000 do x=x+i end return x` -> `:500500`
- table build/sum loop -> `:20100`
- closure capture loop -> array `1,2,3`

Ordering/tie-breaking/RNG/floating-point:

- No store ordering, sorted-set tie-breaking, random-state, or floating-point
  algorithm was touched.
- Lua numeric and error behavior was covered by focused unit tests.
- RNG parity was covered by `math_random_matches_vendored_redislrand48_lwj8o`
  inside the `lua_` focused test run.

## Validation

Passed:

- `cargo fmt -p fr-command -- --check`
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-iror0-pass143-validation-target CARGO_BUILD_JOBS=1 cargo check -j 1 -p fr-command --all-targets`
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-iror0-pass143-validation-target CARGO_BUILD_JOBS=1 cargo test -j 1 -p fr-command --lib lua_ -- --nocapture`
  - 200 passed, 0 failed.
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-iror0-pass143-clippy-target2 CARGO_BUILD_JOBS=1 cargo clippy -j 1 -p fr-command --all-targets -- -D warnings`
- Focused rerun after live-oracle flake:
  `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-iror0-pass143-conformance-rerun-target CARGO_BUILD_JOBS=1 cargo test -j 1 -p fr-conformance --test smoke core_pfdebug_live_redis_matches_runtime -- --nocapture`
  - 1 passed, 0 failed.

Conformance note:

- Full `cargo test -j 1 -p fr-conformance -- --nocapture` completed library,
  binary, and most smoke tests, including `core_scripting` live oracle
  `272/272`, then failed once in unrelated
  `core_pfdebug_live_redis_matches_runtime` with `pfselftest_returns_ok: redis
  did not reply before timeout`. The focused rerun passed, classifying it as a
  transient live Redis timeout rather than a Lua regression.

UBS:

- `ubs crates/fr-command/src/lua_eval.rs artifacts/optimization/frankenredis-iror0/pass143/lua_eval_bench.py`
  returned nonzero with broad historical findings in the large Lua evaluator
  (`305` critical, `3364` warning, `717` info). Its Rust section reported
  formatting, clippy, cargo check, and test-build clean.
- After rewriting helper file writes through explicit context managers,
  `ubs artifacts/optimization/frankenredis-iror0/pass143/lua_eval_bench.py`
  exited 0 with `0` critical, `2` warnings, and `1` info. The remaining
  warnings are the intentional `subprocess.Popen` server lifecycle and
  subprocess import; `main` terminates or kills the process in `finally`.

## Decision

Keep this pass. It is a productive local-frame primitive that removes the
HashMap hashing component from Lua local access and produced a stable
1.37-1.41x gain on the profiled `loop1000` EVAL workload. The remaining gap
must be attacked with a deeper parse-time local slot resolver.
