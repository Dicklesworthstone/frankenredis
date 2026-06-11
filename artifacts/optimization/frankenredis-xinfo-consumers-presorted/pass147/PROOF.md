# frankenredis-ohsk5.30 pass147 proof

## Target

Profile-backed bead: `frankenredis-ohsk5.30`

Fresh stream residual profile on current `origin/main` (`12d57a9c7`) showed
`<fr_store::Store>::xinfo_consumers` at 60.37% children / 36.22% self on the
`pending=50000 consumers=1000` XINFO harness. The associated top self symbol was
`__memcmp_avx2_movbe` at 36.35%, mostly under `xinfo_consumers`.

The code held consumer names in `StreamGroup.consumers: BTreeSet<Vec<u8>>`, then
collected rows in that iteration order and sorted the output vector again:

`result.sort_by(|a, b| a.0.cmp(&b.0))`

## Lever

Remove the redundant final sort in `Store::xinfo_consumers`.

`BTreeSet<Vec<u8>>` iteration is already byte-lexicographic by `Vec<u8>::Ord`,
which is the same ordering the removed comparator used. No command semantics,
row values, tie-breaking rule, floating-point behavior, or RNG behavior is
changed.

## Baseline

Baseline harness binary was built through RCH from the unmodified detached
baseline worktree `/data/projects/.worktrees/frankenredis-coralox-baseline-p147`
at `12d57a9c7`:

`RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/coralox-fr-p147-baseline-harness-target cargo build --release --manifest-path artifacts/optimization/frankenredis-b0exs/pass146/harness/Cargo.toml`

Subagent routing baseline on the same workload:

`--mode xinfo --pending 50000 --consumers 1000 --iters 5000`

Mean: 939.1 ms +/- 23.8 ms.

## Golden Output

XINFO golden SHA256:

| Build | SHA256 |
| --- | --- |
| baseline | `7b4dd8c57c407e08d167a3d279d40922d142c9ee973b9507ad189526051b57b9` |
| candidate | `7b4dd8c57c407e08d167a3d279d40922d142c9ee973b9507ad189526051b57b9` |

XPENDING golden SHA256:

| Build | SHA256 |
| --- | --- |
| baseline | `496b2b6e7955d1dd8e964674586b466a502796af2adb0b4da369d9c898cc6017` |
| candidate | `496b2b6e7955d1dd8e964674586b466a502796af2adb0b4da369d9c898cc6017` |

The XINFO and XPENDING golden outputs are byte-identical to pass146 and to each
other across baseline/candidate.

## Benchmark

Harness:

`--mode xinfo --pending 50000 --consumers 1000 --iters 5000`

Paired order: baseline, then candidate.

| Build | Mean | Stddev |
| --- | ---: | ---: |
| baseline | 0.9522773034371429 s | 0.024373306926264254 s |
| candidate | 0.8865880927228572 s | 0.02139951081439289 s |

Speedup: 1.07x +/- 0.04x.

Reversed order: candidate, then baseline.

| Build | Mean | Stddev |
| --- | ---: | ---: |
| candidate | 1.0878813450628573 s | 0.1938110873361116 s |
| baseline | 1.4588021896342858 s | 0.027033502936061007 s |

Speedup: 1.34x +/- 0.24x.

## Isomorphism Proof

- Consumer ordering is unchanged. Before, output was sorted by `Vec<u8>::cmp`.
  After, output follows `BTreeSet<Vec<u8>>` iteration, which uses the same
  `Vec<u8>::Ord` byte ordering.
- Ties are impossible for consumer names because `BTreeSet` contains unique
  names.
- Pending counts still come from `group_state.pending_count_for_consumer`.
- Idle and inactive fields still come from the same metadata and fallback
  logic.
- Error behavior for missing keys, missing groups, and wrong-type keys is
  untouched.
- No floating-point behavior is involved.
- No RNG behavior is involved.
- The existing unit test inserts `c2` before `c1` and still observes `c1, c2`.

## Post-Keep Profile

Candidate post-profile:

`perf record -F 999 -g --call-graph dwarf -- ... --mode xinfo --pending 50000 --consumers 1000 --iters 10000`

Captured 2026 samples, 0 lost.

Top rows after the sort removal:

| Symbol | Children | Self |
| --- | ---: | ---: |
| `<fr_store::Store>::xinfo_consumers` | 61.92% | 42.74% |
| `__memcmp_avx2_movbe` | 36.06% | 35.21% |
| `xpending_summary_harness::main` | 12.42% | 10.72% |

Next primitive: merge consumer metadata and pending-count state so XINFO can
walk one ordered consumer state map instead of performing multiple per-row
`BTreeMap` lookups and byte comparisons.

## Validation

Passed:

- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/coralox-fr-p147-baseline-harness-target cargo build --release --manifest-path artifacts/optimization/frankenredis-b0exs/pass146/harness/Cargo.toml`
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/coralox-fr-p147-candidate-harness-target cargo build --release --manifest-path artifacts/optimization/frankenredis-b0exs/pass146/harness/Cargo.toml`
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/coralox-fr-p147-frstore-gates-target cargo test -p fr-store xinfo -- --nocapture`
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/coralox-fr-p147-frstore-gates-target cargo check -p fr-store --all-targets`
- `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/coralox-fr-p147-frstore-clippy-target cargo clippy -p fr-store --all-targets -- -D warnings`
- `cargo fmt -p fr-store -- --check`
- `cargo fmt --manifest-path artifacts/optimization/frankenredis-b0exs/pass146/harness/Cargo.toml -- --check`
- `git diff --check`

UBS:

- `ubs crates/fr-store/src/lib.rs` exited 1 on the existing file-wide inventory.
- Embedded UBS sections report formatting clean, no clippy warnings/errors,
  cargo check clean, and tests build clean.
- No UBS finding points at the changed line.

## Score

Impact: 1.07

Confidence: 5

Effort: 1

Score: 5.35

Keep decision: accepted, above the 2.0 threshold.
