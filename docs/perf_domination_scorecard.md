# FrankenRedis Perf-Domination Scorecard (vs redis 7.2.4)

## Targeted Gauntlet: frankenredis-n2u1g ZRANGE WITHSCORES Direct Score Encode

- Commit measured: `0a395dd57` server source (`release-perf`; local binary materialized after
  rch compile completed but did not copy back release-perf executables).
- Build: `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b RUSTFLAGS='-C force-frame-pointers=yes' cargo build --profile release-perf -p fr-server -p fr-bench`, then local materialization into the same target dir.
- Workload: `fr-bench --workload zrange-withscores`, 200k requests, 4 clients, 5 trials,
  p1/p16/p128, fresh ports `43121/43122`, vendored Redis 7.2.4.
- Raw artifact: `artifacts/optimization/frankenredis-n2u1g/verify_zrange_withscores_20260619T0515Z/summary.json`.
- Guard: `zset_score_emit_differ.py` passed byte-exact vs Redis 7.2.4 for ZSCORE/ZMSCORE/ZINCRBY/ZADD-INCR/WITHSCORES/ZPOPMIN/ZPOPMAX under RESP2 and RESP3.
- Keep/revert decision: **KEEP**. Win/loss/neutral `3/0/0`; p16 and p128 are clean low-CV Redis-relative wins.

| depth | Redis ops/s | fr ops/s | fr/redis | cv redis/fr | p99 redis/fr us | verdict |
|---:|--:|--:|--:|--:|--:|---|
| 1 | 65,524 | 71,038 | 1.084 | 5.94/2.58 | 99/83 | fr faster, exact cell noisy |
| 16 | 176,576 | 226,505 | 1.283 | 3.67/1.43 | 486/307 | **fr faster clean** |
| 128 | 188,686 | 259,932 | 1.378 | 0.71/1.54 | 3937/2401 | **fr faster clean** |

## Targeted Gauntlet: frankenredis-uhthd Boxed Keyspace Storage

- Commit measured: pre-commit candidate on top of `c1f8893d`; proof artifact
  `artifacts/optimization/frankenredis-uhthd-boxed-keys/20260619T0557Z/summary.json`.
- Build: `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b
  RUSTFLAGS='-C force-frame-pointers=yes' cargo build --profile release-perf -p fr-server
  -p fr-bench`, then local materialization in the same target dir because rch did not copy back
  custom-target release-perf executables.
- Workload: `scripts/memory_baseline_capture.py`, fresh Redis 7.2.4 and FrankenRedis processes,
  scale 200k, high non-colliding ports.
- Keep/revert decision: **KEEP**. Target keyspace RSS ratio improved from `1.688x` to `1.348x`
  Redis, and FrankenRedis absolute RSS fell in all seven memory cells. Not a closeout:
  keyspace is still a Redis-relative loss.
- Correctness gates: focused `fr-store` keyspace/volatile tests passed, `scan_invariant_gate.py`
  passed, and `cargo test -p fr-conformance -- --nocapture` passed.

| memory cell | baseline fr/redis RSS | post fr/redis RSS | verdict |
|---|--:|--:|---|
| keyspace | 1.688 | 1.348 | target gap shrank 20.1%; Redis still lighter |
| hash | 1.474 | 1.239 | improved; still loss |
| list | 1.177 | 1.169 | neutral/improved; still loss |
| set | 1.107 | 1.184 | fr RSS improved; ratio hurt by Redis RSS variance |
| string_1k | 0.951 | 0.892 | fr win |
| stream | 0.981 | 0.978 | fr win |
| zset | 1.795 | 1.883 | fr RSS improved; ratio hurt by Redis RSS variance |

Win/loss/neutral vs Redis on memory after lever: **2/5/0**. Absolute FrankenRedis RSS
delta across cells: **7/0/0** improved/regressed. Throughput smoke: SET `1.02x`, GET `0.94x`,
HSET `1.06x`, ZADD `0.84x`; with neutral band 0.90-1.00x, **2/1/1**. ZADD remains a gap.

## Targeted Gauntlet: frankenredis-uhthd Lazy Sorted Key Index

- Commit measured: `4cf73ebef`
- Build: `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo build --release -p fr-server -p fr-bench`
- Memory harness: fixed high-port run, `FR_BENCH_PORT_BASE=42051`, 200k scale.
- Keyspace RSS: `fr_rss=30515200`, `redis_rss=15958016`, fr/redis `1.912x`.
- Keep/revert decision: **KEEP**. This is not domination yet, but it improves the prior documented `uhthd` residual of `2.59x`; no revert.
- SCAN guard: `scan_invariant_gate.py` passed.
- Targeted 100k-key timing: SET load `0.963x`; first full `SCAN COUNT 1000` `0.985x`; warm full SCAN `1.039x`.
- Measurement caveat: old `299xx` runs can collide with peer benchmark ports and produce invalid all-equal RSS. Use distinct high-port pairs via `FR_BENCH_PORT_BASE`.

## Throughput (fr/redis ops/sec; >=1.0 = fr wins)

- Cells rated: **3** (excluding 33 noisy cv>5% cells)
- fr wins (>=1.0x): **1/3** (33%)
- Throughput geomean: **0.981x**

| workload@depth | fr/redis | fr cv% | verdict |
|---|---|---|---|
| hset@p1 | 0.901 | 4.3 | loss |
| incr@p1 | 0.993 | 2.11 | loss |
| set@p1 | 1.054 | 2.81 | WIN |

**Throughput gaps (fr slower):** hset@p1=0.90x, incr@p1=0.99x

_Noisy (excluded): dump@p1, dump@p128, dump@p16, get@p1, get@p128, get@p16, hget@p1, hget@p128, hget@p16, hgetall@p1, hgetall@p128, hgetall@p16, hset@p128, hset@p16, incr@p128, incr@p16, lpush@p1, lpush@p128, lpush@p16, lrange@p1, lrange@p128, lrange@p16, mixed@p1, mixed@p128, mixed@p16, set@p128, set@p16, smembers@p1, smembers@p128, smembers@p16, zrange-withscores@p1, zrange-withscores@p128, zrange-withscores@p16_

## Throughput — latest release sweep (`artifacts/optimization/coralox-pass195-current-main-profile/standard_sweep_p16_c50_n300k_reps5.txt`)

- Commands: **19**, fr faster on **13/19**
- fr/redis geomean: **1.059x**

| command | fr/redis | verdict |
|---|---|---|
| get | 1.173 | fr-faster |
| hset | 1.096 | fr-faster |
| incr | 0.997 | FR-SLOWER |
| lpop | 1.134 | fr-faster |
| lpush | 1.034 | fr-faster |
| lrange_100 | 0.987 | FR-SLOWER |
| lrange_300 | 1.011 | fr-faster |
| lrange_500 | 0.938 | FR-SLOWER |
| lrange_600 | 1.038 | fr-faster |
| mset | 1.375 | fr-faster |
| ping_inline | 1.028 | fr-faster |
| ping_mbulk | 0.943 | FR-SLOWER |
| rpop | 1.145 | fr-faster |
| rpush | 0.963 | FR-SLOWER |
| sadd | 1.019 | fr-faster |
| set | 1.243 | fr-faster |
| spop | 0.993 | FR-SLOWER |
| zadd | 1.090 | fr-faster |
| zpopmin | 1.014 | fr-faster |

**Throughput gaps (fr slower in this sweep):** lrange_500=0.94x, ping_mbulk=0.94x, rpush=0.96x, lrange_100=0.99x, spop=0.99x, incr=1.00x

_Note: a point-in-time artifact sweep, not the ratcheted .bench-history baseline; run perf_baseline_capture.py for the keep-gated matrix._

## Memory (fr/redis RSS; <=1.0 = fr wins)

- Types rated: **7**
- fr wins (<=1.0x RSS): **2/7**
- RSS geomean: **1.210x**

| data-type | fr/redis RSS | fr/redis used_memory | verdict |
|---|---|---|---|
| hash | 1.239 | 0.838 | loss |
| keyspace | 1.348 | 0.805 | loss |
| list | 1.169 | 0.391 | loss |
| set | 1.184 | 0.562 | loss |
| stream | 0.978 | 1.096 | WIN |
| string_1k | 0.892 | 0.964 | WIN |
| zset | 1.883 | 0.620 | loss |

**RAM gaps (fr heavier):** zset=1.88x, keyspace=1.35x, hash=1.24x, set=1.18x, list=1.17x.
The zset ratio worsened in the latest run because Redis RSS fell more than fr; fr absolute RSS
improved by 73,728 B in that cell.
