# frankenredis-ohsk5.57 pass190 rejection report

## Target

- Bead: `frankenredis-ohsk5.57`
- Profile-backed hotspot: child-owned GDB userspace sample placed `hashbrown::HashMap::remove` for `Store::hll_register_cache` inside `Store::incrby_existing_or_insert` on `INCR` P16/C50.
- Attempted one lever: skip the `hll_register_cache.remove(key)` call after successful numeric `INCR`/`INCRBY`, relying on `Entry::modification_count` to make any stale HLL register sidecar unobservable.

## Baseline

- Source: `331491f35a4c16039a0fecdffd122de5f7dc393d`
- Build: `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=target-ohsk5-57-baseline CARGO_BUILD_JOBS=1 cargo build -j 1 --profile release-perf -p fr-server -p fr-bench`
- RCH worker: `vmi1156319`
- Fixed-server hyperfine, `INCR`, clients 50, pipeline 16, requests 300000, keyspace 100000:
  - Mean: `531.1 ms +/- 94.0 ms`
  - Range: `430.9 ms .. 683.0 ms`
- Last fr-bench JSON: `558600.079 ops/s`, p50 `1232 us`, p95 `1845 us`, p99 `10567 us`.

## Candidate

- Source: same base plus only the attempted `fr-store` HLL remove skip.
- Build: `RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=target-ohsk5-57-candidate CARGO_BUILD_JOBS=1 cargo build -j 1 --profile release-perf -p fr-server -p fr-bench`
- RCH worker: `vmi1152480`
- Fixed-server hyperfine, same workload:
  - Mean: `562.5 ms +/- 75.9 ms`
  - Range: `518.3 ms .. 696.5 ms`
- Last fr-bench JSON: `583117.347 ops/s`, p50 `1176 us`, p95 `1571 us`, p99 `10575 us`.

## Paired Check

To reduce cross-run state effects, a paired hyperfine run started a fresh server per measured run and used the same baseline `fr-bench` binary for both commands.

- Baseline paired mean: `731.2 ms +/- 31.9 ms`
- Candidate paired mean: `703.9 ms +/- 30.3 ms`
- Paired ratio: `1.04x +/- 0.06x`

## Behavior Proof

- Focused rch test on `vmi1156319`: `cargo test -j 1 -p fr-store incrby_existing_key_matches_entry_replacement_side_effects -- --nocapture` passed.
- Golden RESP transcript SHA256:
  - Baseline: `ddd8e43f40c91caf9ffb8d58387f08e83da8f2af0d548f1c66a3d9f7b53e4a50`
  - Candidate: `ddd8e43f40c91caf9ffb8d58387f08e83da8f2af0d548f1c66a3d9f7b53e4a50`
- Transcript covers: `SET`, `INCR`, `INCRBY`, `PFADD`, single-key `PFCOUNT`, multi-key `PFCOUNT`, HLL key overwrite, numeric `INCR` after overwrite, invalid HLL `PFCOUNT`, and invalid integer `INCR`.
- Isomorphism: command ordering, reply ordering, HLL cardinality/tie behavior, floating-point behavior, and RNG behavior are unchanged. The attempted private-cache difference is guarded by `Entry::modification_count` and is not serialized or externally observable.

## Decision

- Decision: reject and do not keep the code change.
- Score: `Impact 1.04 x Confidence 0.45 / Effort 0.50 = 0.94`, below the `>=2.0` keep gate.
- Reason: fixed-server hyperfine was slower, and the paired run only showed a small/noisy `1.04x` edge. This is insufficient evidence for a retained performance commit.
- Source status: attempted `fr-store` change reverted; this bead is closed with evidence only.

## Next Route

This rejection points away from HLL-cache micro-tuning. The next optimization route should attack a deeper no-gaps primitive in the RESP/parser/event-loop path: per-batch arena/slab reuse for command token storage, zero-copy frame scanning, or branchless borrowed-command dispatch. Target ratio should be at least `1.20x` on the same P16/C50 command mix before considering commit retention.
