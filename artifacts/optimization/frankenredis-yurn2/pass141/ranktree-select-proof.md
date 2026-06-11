# frankenredis-yurn2 pass 141 proof

Target: `ZRANDMEMBER z -100` over a 100k sorted set after the rank tree is warmed by `ZRANK`.

Lever: `SortedSet::members_at_indices` uses `ZRankTreap::select(idx)` when the full sorted set already has a rank tree. Cold/packed sorted sets keep the prior single ordered pass.

## Baseline

Source head: `6a660fa99`

Build:
`RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-yurn2-baseline CARGO_BUILD_JOBS=1 cargo build -j 1 -p fr-server --profile release-perf`

Worker: `vmi1227854`

Hyperfine:
`5.510416816826667s +/- 0.11669973213826317s`

Harness internal:
`200.26202230729265 ops/s`, `4993.458013050258 us/op`

## Candidate

Source worktree: `/data/projects/.scratch/frankenredis-yurn2-codex`

Build:
`RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-yurn2-codex/target-yurn2-candidate CARGO_BUILD_JOBS=1 cargo build -j 1 -p fr-server --profile release-perf`

Worker: `vmi1227854`

Hyperfine:
`2.053735490953333s +/- 0.00943233126013078s`

Harness internal:
`553.9352742508031 ops/s`, `1805.265067028813 us/op`

Speedup:
Hyperfine `2.682x`; internal steady-state `2.766x`.

Score:
Impact `2.76` x Confidence `0.95` / Effort `1.0` = `2.62`; keep.

## Golden parity

Behavior SHA:
`6feaa20692bb5f67ae546ff28a0b921657e5b6a045b9aa35875e829b88a7d9e3` baseline = candidate.

Workload raw transcript SHA:
`2f0ea2079cff21531893345c5309efdfed2634036070a367fc0f62206d3603d0` baseline = candidate.

Observed reply lengths:
`[100, 100, 100, 100, 100]` baseline = candidate.

## Isomorphism proof

Ordering:
The caller still draws all random indices before materializing members. The fast path maps each requested index in the original `indices` order.

Duplicates:
The fast path iterates `indices` directly, so duplicate picks produce duplicate output entries exactly as before.

Out-of-range:
`tree.select(idx)` returns `None`; `filter_map` skips it, matching the previous `HashMap::get(idx).cloned()` skip.

Tie-breaking:
`ZRankTreap` stores the same `ScoreMember` ordering as the BTreeMap `(score, member)` order. Member bytes and scores are cloned from the selected `ScoreMember`.

Floating point:
No score arithmetic, comparison, normalization, or formatting changed.

RNG:
`Store::zrandmember_count` index generation is unchanged; only post-draw member lookup changes.

Cold behavior:
Packed sorted sets and full sorted sets without `rank_tree` still use the previous single-pass `iter_asc().enumerate()` fallback.

## Validation

Baseline/candidate benchmark:
Local `hyperfine` against RCH-built binaries. RCH refused non-compilation hyperfine with `remote required; refusing local fallback`, so only compilation was remote.

Tests:
`RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-yurn2-codex/target-yurn2-test3 CARGO_BUILD_JOBS=1 cargo test -j 1 -p fr-store --lib members_at_indices_isomorphic_and_faster_zrnd1 -- --nocapture`

Result:
1 passed, 0 failed. The crate still emits pre-existing test-only warnings near older proof helpers.

Check:
`RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-yurn2-codex/target-yurn2-check CARGO_BUILD_JOBS=1 cargo check -j 1 -p fr-store --all-targets`

Result:
Passes; same pre-existing test-only warnings.

Clippy:
`RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-yurn2-codex/target-yurn2-clippy CARGO_BUILD_JOBS=1 cargo clippy -j 1 -p fr-store --lib -- -D warnings`

Result:
Passes.

Format:
`cargo fmt -p fr-store --check`

Result:
Passes.

UBS:
`ubs --only=rust --format=json --comparison artifacts/optimization/frankenredis-yurn2/pass141/ubs-baseline.json --report-json artifacts/optimization/frankenredis-yurn2/pass141/ubs-candidate-comparison.json crates/fr-store/src/lib.rs`

Result:
Exits nonzero on the historical full-file inventory, but candidate totals match baseline exactly: 208 critical, 5474 warning, 917 info.
