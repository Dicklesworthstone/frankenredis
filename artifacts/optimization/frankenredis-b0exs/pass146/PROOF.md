# frankenredis-b0exs pass146 proof

## Target

Profile-backed bead: `frankenredis-b0exs`

Initial hot path: `fr-store` `XPENDING` summary on a large stream PEL.

Post-rebase shifted hot path: `fr-store` `XINFO CONSUMERS` on the same large PEL. `origin/main` already memoized repeated `XPENDING` summaries, but `XINFO CONSUMERS` still rescanned the whole PEL per consumer for pending counts and last-delivery fallback.

Lever: maintain a per-consumer pending-count sidecar in `StreamGroup`, update it at every PEL mutation, use it for stream consumer summaries, and avoid the last-delivery fallback scan when restored/live consumer metadata already has `seen_time_ms`.

## Harness

`artifacts/optimization/frankenredis-b0exs/pass146/harness`

Workload:

`pending=50000 consumers=1000`

Release harness binaries were built through RCH. After `origin/main` advanced, the fresh baseline binary was built from detached worktree `/data/projects/.scratch/frankenredis-b0exs-origin-62f285cf8` at `62f285cf8`; the candidate binary was built from rebased `HEAD`.

## Current Keep Gate

XINFO command:

`--mode xinfo --pending 50000 --consumers 1000 --iters 20`

Paired hyperfine against current `origin/main`:

| Build | Mean | Stddev |
| --- | ---: | ---: |
| origin/main peer baseline | 9.16553930978 s | 0.346439391719385 s |
| rebased candidate | 0.043570660580000004 s | 0.002973242860963633 s |

Speedup: 210.36x +/- 16.41x.

Direct loop timing:

| Build | elapsed_ns | checksum |
| --- | ---: | ---: |
| origin/main peer baseline | 8718376110 | 123804727951931195330734227879209542864 |
| rebased candidate | 3713267 | 123804727951931195330734227879209542864 |

## Golden Output

XINFO golden SHA256:

| Build | SHA256 |
| --- | --- |
| origin/main peer baseline | `7b4dd8c57c407e08d167a3d279d40922d142c9ee973b9507ad189526051b57b9` |
| rebased candidate | `7b4dd8c57c407e08d167a3d279d40922d142c9ee973b9507ad189526051b57b9` |

XPENDING golden SHA256 also remained unchanged:

`496b2b6e7955d1dd8e964674586b466a502796af2adb0b4da369d9c898cc6017`

Golden outputs are byte-identical.

## XPENDING Post-Rebase Check

After peer commit `62f285cf8`, repeated `XPENDING` summary was already memoized. The rebased sidecar path is intentionally not scored on this repeat workload:

| Build | Mean | Stddev |
| --- | ---: | ---: |
| origin/main peer baseline | 0.07597650094571429 s | 0.0015956266259780955 s |
| rebased candidate | 0.07653279080285715 s | 0.0030839215497548682 s |

This is a tie within noise, so the keep decision is based on the shifted XINFO hotspot above.

## Initial Pre-Rebase Routing Evidence

Before `origin/main` advanced with the peer cache commit, the same maintained-count sidecar moved repeated `XPENDING` summary from `4.85956959962 s +/- 0.03423631412618061 s` to `0.07680014433428571 s +/- 0.0034093903656901648 s` on the same harness, a 63.28x +/- 2.84x routing win. That number is retained as historical routing evidence only.

## Isomorphism Proof

- Consumer ordering is preserved. Both old and new paths emit `BTreeMap` byte-lexicographic consumer order.
- `XPENDING` first ID, last ID, and total pending count still come directly from the authoritative PEL map.
- `XINFO CONSUMERS` row order still follows the existing consumer-name sort.
- Per-consumer counts are updated on every PEL mutation path touched by streams: `XREADGROUP` new delivery insert, `XCLAIM` owner transfer and force insert, missing-record cleanup, `XAUTOCLAIM` owner transfer and deleted-ID cleanup, `XACK`, `XGROUP DELCONSUMER`, group creation, and restore/load.
- Debug builds assert the sidecar equals a recomputed count from the authoritative PEL before summary emission.
- Idle/inactive behavior is unchanged: when `seen_time_ms > 0`, the old fallback `last_delivery` scan result was ignored; when metadata is missing or restored with `seen_time_ms == 0`, the code still performs the same fallback scan.
- No floating-point behavior is involved.
- No RNG behavior is involved.
- Tie-breaking is unchanged because stream IDs and consumer names keep their existing `BTreeMap` ordering.

## Validation

Passed after the post-rebase XINFO adjustment:

- `RCH_REQUIRE_REMOTE=1 rch exec -- cargo test -p fr-store stream_ -- --nocapture`
- `RCH_REQUIRE_REMOTE=1 rch exec -- cargo check -p fr-store --all-targets`
- `RCH_REQUIRE_REMOTE=1 rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`
- `cargo fmt -p fr-store --check`
- `cargo fmt --manifest-path artifacts/optimization/frankenredis-b0exs/pass146/harness/Cargo.toml --check`
- `git diff --check`
- `ubs crates/fr-store/src/lib.rs`

UBS exited 0 and recorded the existing broad `fr-store` inventory in `ubs.log`.

## Score

Impact: 210.36

Confidence: 0.95

Effort: 2

Score: 99.92

Keep decision: accepted, above the 2.0 threshold.
