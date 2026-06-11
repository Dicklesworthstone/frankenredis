# frankenredis-fmilu pass143 proof

## Target

- Bead: `frankenredis-fmilu`
- Profile-backed hotspot: trivial `EVAL "return 1" 0` pays per-call Lua environment rebuild cost.
- Lever: reuse readonly standard-library and `redis` template tables across `LuaState` instances, and lazily materialize `_G` only when the script observes the default environment.

## Benchmark

Harness:

```text
python3 artifacts/optimization/frankenredis-fmilu/pass143/eval_trivial_harness.py \
  --server-bin <frankenredis> \
  --out-dir artifacts/optimization/frankenredis-fmilu/pass143 \
  --ops 10000 --warmup-ops 200 --trials 1
```

Baseline build:

```text
RCH_REQUIRE_REMOTE=1 rch exec -- env \
  CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-yurn2-codex/target-fmilu-baseline \
  CARGO_BUILD_JOBS=1 cargo build -j 1 -p fr-server --profile release-perf
```

Final candidate build:

```text
RCH_REQUIRE_REMOTE=1 rch exec -- env \
  CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-yurn2-codex/target-fmilu-candidate3 \
  CARGO_BUILD_JOBS=1 cargo build -j 1 -p fr-server --profile release-perf
```

Results:

| artifact | hyperfine mean | stddev | ops/s sample | us/op sample |
|---|---:|---:|---:|---:|
| baseline | 1.2324441059 s | 0.0388494409 s | 9250.0366 | 108.1077 |
| candidate-final | 0.8592647455 s | 0.0449385147 s | 13726.2759 | 72.8530 |

- Hyperfine speedup: 1.4343007931x
- Internal last-trial speedup: 1.4839158476x
- Score: Impact 1.4343 x Confidence 0.97 / Effort 0.65 = 2.14

## Golden output

Hot workload raw RESP transcript:

```text
47b06206db9f0cba69a9cd23108db88dfce38549ed98d79da135a5ce08853951
```

PING behavior transcript:

```text
e4d5419d2f9ea7c33e1ce857b1298a1a8a9ce1836c25273f146618e4bbc00ced
```

Environment golden transcript (`_G`, `pairs(_G)` order, `getfenv`, readonly `redis`, KEYS/ARGV, `math.random`, coroutine):

```text
96765b2f06107f18857d3d5360ea321caa4ad161c8d25b7537bf02a8ea372758
```

Baseline and candidate-final hashes match for all three transcript classes.

## Isomorphism proof

- Ordering: command execution order is unchanged. `_G` materialization is lazy, but the golden transcript includes exact `pairs(_G)` order and matches baseline byte-for-byte.
- Tie-breaking: no data-structure comparator, sorted-set, hash-slot, or command-dispatch tie-break path changed.
- Floating point: `math.random` state remains per-`LuaState`; shared tables store only function names/constants. The golden transcript pins `math.randomseed(1)` output.
- RNG: `RedisLrand48` initialization and mutation stay per call; no shared RNG state was introduced.
- Mutability: shared library tables are marked readonly before script code can observe them. KEYS/ARGV remain per-call writable tables. `redis` was already readonly before user mutation; sharing it only removes per-call construction.
- Lifetime: shared template tables are not registered in the eval cycle-break registry and are skipped by per-state teardown. Per-script tables still participate in the existing cycle cleanup, proven by `lua_cyclic_scripts_do_not_leak_qqq17`.

## Validation

- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=... cargo test -j 1 -p fr-command --lib lua_ -- --nocapture`
  - final source: 200 passed, 0 failed on `vmi1152480`
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=... cargo check -j 1 -p fr-command --all-targets`
  - passed on `vmi1227854`
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=... cargo clippy -j 1 -p fr-command --lib -- -D warnings`
  - passed on `vmi1227854`
- `cargo fmt -p fr-command --check`
  - passed
- `git diff --check`
  - passed
- `ubs --only=rust crates/fr-command/src/lua_eval.rs crates/fr-command/src/lib.rs`
  - exited nonzero on historical inventory; formatter/clippy/build sections are clean. Summary: 800 critical, 22555 warning, 1820 info across the two files, dominated by pre-existing test panic/unwrap/assert inventory.

## Notes

- `crates/fr-command/src/lib.rs` changed only because `cargo fmt -p fr-command` normalized existing formatting in that crate.
- RCH daemon was restarted once after the scheduler reported `no admissible workers: critical_pressure=2`; subsequent remote-only validation resumed normally.
