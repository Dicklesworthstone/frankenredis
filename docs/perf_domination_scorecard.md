# FrankenRedis Perf-Domination Scorecard (vs redis 7.2.4)

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
- RSS geomean: **1.315x**

| data-type | fr/redis RSS | fr/redis used_memory | verdict |
|---|---|---|---|
| hash | 1.426 | 0.838 | loss |
| keyspace | 1.912 | 0.805 | loss |
| list | 1.212 | 0.391 | loss |
| set | 1.199 | 0.562 | loss |
| stream | 0.988 | 1.096 | WIN |
| string_1k | 0.942 | 0.964 | WIN |
| zset | 1.841 | 0.62 | loss |

**RAM gaps (fr heavier):** keyspace=1.91x, zset=1.84x, hash=1.43x, list=1.21x, set=1.20x
