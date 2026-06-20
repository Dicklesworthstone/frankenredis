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

## Throughput (fr-bench matrix vs Redis 7.2.4; >=1.0 = fr wins)

- Latest capture: `.bench-history/comprehensive_bench.latest.json`, `trials=7`, `requests=200000`.
- Cells rated: **15** stable cells (excluding 23 noisy `fr_cv_pct>5%` cells and 1 skipped cell).
- Stable score: **7 wins / 6 losses / 2 neutral**.
- Stable-cell throughput geomean: **0.952x**.
- Ratchet status: **FAIL** vs prior baseline; regressions were `integer-get@p1`, `lpush@p1`,
  `dump@p1`, `dump@p128`, and `mixed@p16`.

| workload@depth | fr/redis | fr cv% | verdict |
|---|---:|---:|---|
| dump@p1 | 0.716 | 1.81 | loss |
| dump@p128 | 0.375 | 3.74 | loss |
| get@p1 | 1.069 | 3.06 | WIN |
| hget@p1 | 0.937 | 4.99 | loss |
| hgetall@p16 | 1.321 | 3.76 | WIN |
| incr@p1 | 0.959 | 3.60 | loss |
| integer-get@p1 | 1.024 | 3.84 | neutral |
| lpush@p1 | 0.806 | 3.98 | loss |
| lrange@p1 | 1.122 | 2.62 | WIN |
| lrange@p128 | 1.966 | 4.42 | WIN |
| mixed@p16 | 0.347 | 1.80 | loss |
| set@p1 | 1.008 | 3.83 | neutral |
| set@p128 | 1.704 | 4.60 | WIN |
| smembers@p1 | 1.218 | 2.90 | WIN |
| zrange-withscores@p1 | 1.064 | 4.06 | WIN |

**Stable throughput gaps:** dump@p128=0.38x, mixed@p16=0.35x, dump@p1=0.72x,
lpush@p1=0.81x, hget@p1=0.94x, incr@p1=0.96x.

_Noisy (excluded): dump@p16, get@p128, get@p16, hget@p128, hget@p16, hgetall@p1,
hgetall@p128, hset@p1, hset@p128, hset@p16, incr@p128, incr@p16, integer-get@p128,
integer-get@p16, lpush@p128, lpush@p16, lrange@p16, mixed@p1, set@p16, smembers@p128,
smembers@p16, zrange-withscores@p128, zrange-withscores@p16. Skipped: mixed@p128._

## Focused pass195 residual confirmation (`frankenredis-15lug`, Redis C client)

- Artifact: `artifacts/optimization/frankenredis-15lug-cv-confirm/20260620T042556Z/redis_benchmark_p16_c50_n150k_trials7.txt`.
- Harness: vendored `redis-benchmark`, P16, c50, n150k, 7 interleaved trials, current HEAD before
  the rejected candidate.
- Focused score by 3% band: **5 wins / 1 loss / 3 neutral**.
- Parity-floor losses (`<0.9x`): **spop only**, at 0.81x.

| command | median fr/redis | verdict |
|---|---:|---|
| incr | 1.12 | WIN |
| lpush | 0.91 | neutral |
| rpush | 1.03 | WIN |
| spop | 0.81 | loss |
| lrange_100 | 1.08 | WIN |
| lrange_500 | 1.24 | WIN |
| lrange_600 | 1.15 | WIN |
| ping_inline | 1.01 | neutral |
| ping_mbulk | 0.93 | neutral |

Rejected candidate: an early return in `Store::drop_if_expired` for missing keys did not improve
the focused `SPOP` loss (`spop` stayed 0.81x) and introduced focused `lpush`/`rpush` losses in the
candidate sweep, so the source hunk was reverted.

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
- Latest sample: reverted control after rejecting `frankenredis-uhthd` inline-small `StoreKey`
  (`artifacts/optimization/frankenredis-uhthd-smallkey/20260620T0001Z/summary.json`).

| data-type | fr/redis RSS | fr/redis used_memory | verdict |
|---|---|---|---|
| hash | 1.375 | 0.838 | loss |
| keyspace | 1.246 | 0.805 | loss |
| list | 1.206 | 0.391 | loss |
| set | 1.222 | 0.562 | loss |
| stream | 0.979 | 1.096 | WIN |
| string_1k | 0.893 | 0.964 | WIN |
| zset | 1.720 | 0.620 | loss |

**RAM gaps (fr heavier):** zset=1.72x, hash=1.38x, keyspace=1.25x, set=1.22x, list=1.21x.
The latest `uhthd` inline-small-key layout did not ship: it regressed keyspace RSS from 1.169x
to 1.465x in the direct A/B and worsened six of seven absolute FrankenRedis RSS cells.

## Targeted Gauntlet: frankenredis-uhthd Inline-Small StoreKey

- Commit candidate: none kept. The candidate changed `StoreKey` from `Box<[u8]>` to an enum that
  inlined keys up to 15 bytes and heap-boxed longer keys; the production hunk was reverted.
- Build/bench: `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`,
  `rch exec -- cargo build --release -p fr-server -p fr-bench`, followed by
  `scripts/memory_baseline_capture.py` against vendored Redis 7.2.4.
- Proof bundle:
  `artifacts/optimization/frankenredis-uhthd-smallkey/20260620T0001Z/summary.json`.
- Validation after revert: `cargo fmt --check`, workspace `cargo check`, workspace `cargo clippy
  -- -D warnings`, and `cargo test -p fr-conformance -- --nocapture` passed.

| memory cell | baseline fr/redis RSS | candidate fr/redis RSS | candidate absolute fr RSS delta | verdict |
|---|---:|---:|---:|---|
| keyspace | 1.169 | 1.465 | +2,883,584 B | rejected target regression |
| string_1k | 0.879 | 0.894 | +90,112 B | worse |
| list | 1.186 | 1.399 | +90,112 B | worse |
| hash | 1.392 | 1.410 | +208,896 B | worse |
| set | 1.075 | 1.243 | +294,912 B | worse |
| zset | 1.834 | 1.579 | -405,504 B | lone win |
| stream | 0.974 | 0.977 | +585,728 B | worse |

Lever score: **1/6/0** absolute FrankenRedis RSS win/loss/neutral. Redis-relative memory score
after reverting remains **2/5/0** vs Redis 7.2.4; `uhthd` is still open, but this key-inlining
shape is negative evidence.

## Targeted Gauntlet: frankenredis-upx5x EXISTS Encoded Reply

- Commit candidate: borrowed `EXISTS` `_into` path + server `FastEncodedReply` wiring.
- Build/bench: `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b/local-f20a92ec0`,
  `cargo bench -p fr-bench --bench exists_vs_redis -- --noplot`, Redis 7.2.4 oracle from
  `legacy_redis_code/redis/src/redis-server`.
- RCH note: requested-root `rch exec` builds were attempted but failed open after worker sync
  timeouts and mixed-nightly metadata in the shared target; the accepted timing run used the
  compiler-scoped subtarget under the requested root.
- Proof bundle:
  `artifacts/optimization/frankenredis-upx5x/20260619T1803Z/summary.json`.

| workload | fr/redis control | fr/redis candidate | fr candidate/control | verdict |
|---|---:|---:|---:|---|
| exists8_all_hit | 0.719 | 0.808 | 1.149 | keep; Redis still faster |
| exists8_half_hit | 0.768 | 0.803 | 1.239 | keep; Redis still faster |
| exists8_duplicates | 0.785 | 0.895 | 1.317 | keep; Redis still faster |

Redis-relative score after this lever: **0/3/0** wins/losses/neutral. The `EXISTS` loss remains
open, but the encoded reply path narrows all three focused cells and is a measured keeper.

## Targeted Gauntlet: frankenredis-qk0nm EXISTS Runtime Accounting

- Commit candidate: none kept. All runtime/store accounting experiments were reverted.
- Build/bench: `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b/local-f20a92ec0-qk0nm`,
  `cargo bench -p fr-bench --bench exists_vs_redis -- --noplot`, Redis 7.2.4 oracle from
  `legacy_redis_code/redis/src/redis-server`.
- RCH note: `rch exec -- cargo build --release -p fr-server -p fr-bench` succeeded on `hz1`;
  remote `cargo bench` failed because `FR_SERVER_BIN` was rewritten to a bench target that did not
  contain `release/frankenredis`. The shared requested target also contained mixed-nightly metadata,
  so the measured local fallback used the compiler-scoped subtarget under the requested root.
- Proof bundle:
  `artifacts/optimization/frankenredis-qk0nm/20260619T1842Z/summary.json`.

| candidate | all-hit fr/redis | half-hit fr/redis | duplicate fr/redis | verdict |
|---|---:|---:|---:|---|
| control after upx5x | 0.864 | 0.874 | 0.763 | baseline |
| small integer reply table | 0.754 | 0.812 | 0.839 | rejected; fr absolute throughput regressed |
| runtime exact-8 unroll | 0.777 | 0.755 | 0.769 | rejected; fr absolute throughput regressed |
| batch `exists_many_no_touch` | 0.812 | 0.812 | 0.835 | rejected; no credible same-control win |
| exact-8 batch helper | 0.789 | 0.807 | 0.822 | rejected; fr absolute throughput regressed |

Redis-relative score remains **0/3/0** wins/losses/neutral. qk0nm added negative evidence only:
small integer reply tables, exact-8 unrolling, and batch hit/miss aggregation are not viable next
steps for the remaining `EXISTS` loss without new profile evidence.

## Targeted Gauntlet: frankenredis-h6ppr RESP CRLF Scanner

- Commit candidate: none kept. The `fr-protocol::read_line` `memchr::memchr` scanner was measured
  against a HEAD-minus-h6ppr control and reverted.
- Build/bench: current and control release binaries were built with `rch exec` under
  `/data/projects/.rch-targets/frankenredis-cod-a` and
  `/data/projects/.rch-targets/frankenredis-cod-a-h6ppr-control`; Redis 7.2.4 oracle was
  `legacy_redis_code/redis/src/redis-server`.
- Proof bundle:
  `artifacts/optimization/frankenredis-h6ppr/verify_memchr_crlf_20260619T234447Z/summary.json`.
- Profiling note: kernel `perf` was blocked by `perf_event_paranoid=4`.

Initial Redis-relative GET/SET sweep showed current FrankenRedis faster than Redis in all four
cells, but current/control was noisy. Low-CV confirmation rejected the lever:

| workload | current/control | verdict |
|---|---:|---|
| get_p16 | 0.999 | neutral |
| set_p16 | 1.018 | small win |
| get_p128 | 0.959 | rejected regression |
| set_p128 | 0.998 | neutral |

Lever score: **1/1/2** win/loss/neutral. The parser line-scanner rewrite is not a contributor to
the project’s GET/SET Redis-relative wins and should not be retried without fresh profile evidence.

## Cod-b cached borrowed write-gate verification (MEASURED 2026-06-20)

Follow-up for `frankenredis-ohsk5`: the previously coded cached borrowed write gate
(`d14e2b330`) is no longer pending. Current `HEAD` was measured against an inverse-control worktree
with only that commit reverted, and against vendored Redis 7.2.4.

| Gate | Workload | Ratio | CV / trial quality | Verdict |
|---|---|---:|---|---|
| current / inverse-control (`fr-bench`) | SET P16 | 1.117x | current 2.93%, control 4.00% | keep-grade win |
| current / inverse-control (`fr-bench`) | HSET P16 | 1.058x | current 8.92%, control 3.04% | noisy support only |
| current / inverse-control (`redis-benchmark`) | SET/HSET/MSET P16 | 1.05x / 0.99x / 1.01x | 7 interleaved trials | no regression, limited claim |
| current / Redis 7.2.4 (`redis-benchmark`) | SET/HSET/MSET P16 | 1.02x / 0.95x / 0.96x | 7 interleaved trials | parity floor, not domination |

Decision: keep the cached gate. It delivers a clean SET P16 current/control win and does not show a
focused write-family regression, but HSET/MSET remain Redis-relative losses by the 3% score band.

Latest broad quick matrix from `.bench-history/comprehensive_bench.latest.json`:

| score set | wins | losses | neutral | notes |
|---|---:|---:|---:|---|
| all 39 cells | 22 | 15 | 2 | 34 cells exceeded 5% fr CV; route-only evidence |
| stable cells only | 3 | 2 | 0 | wins: GET@P1, INTEGER-GET@P1, SET@P1; losses: INCR@P1, MIXED@P1 |

Release-performance frontier after this pass: `MIXED@P1` is the largest stable loss
(`0.434x` fr/redis), then `INCR@P1` (`0.951x`). Noisy P16/P128 losses need a quieter rerun before
they are valid code targets.

## Cod-b HSET Direct Histogram Candidate (MEASURED 2026-06-20)

Follow-up for `frankenredis-ohsk5`: a dedicated `hset` command histogram slot was tested to bypass
the fallback commandstats `HashMap` lookup. The candidate was reverted because the same-control
A/B showed no clean win.

| Gate | Cell | Ratio | CV / trial quality | Verdict |
|---|---|---:|---|---|
| candidate / baseline (`fr-bench`) | HSET P1 | 0.993x median | all CV < 5% | rejected: clean neutral/slight down |
| candidate / baseline (`fr-bench`) | HSET P16 | 1.202x median | 0/2 clean runs | noisy, not keep evidence |
| candidate / baseline (`fr-bench`) | HSET P128 | 1.068x median | 0/2 clean runs | noisy, not keep evidence |

Lever score: **0/0/2 clean win/loss/neutral**, plus **4 noisy** runs. No source hunk remains.
Proof bundle: `artifacts/optimization/frankenredis-ohsk5-hset-direct-hist/20260620T022647Z/`.

Clean-source focused current-vs-Redis check after revert, built via
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`:

| cell | fr/redis | cv redis/fr | verdict |
|---|---:|---|---|
| `mixed@p1` | 1.031 | 2.21% / 5.69% | noisy, not a clean loss |
| `mixed@p16` | 1.215 | 8.09% / 9.23% | noisy |
| `incr@p1` | 0.954 | 3.55% / 3.39% | clean loss |
| `incr@p16` | 1.144 | 6.41% / 9.14% | noisy |
| `get@p1` | 1.034 | 2.81% / 3.29% | clean win |
| `set@p1` | 0.993 | 2.86% / 4.86% | neutral |
| `hset@p1` | 0.995 | 3.06% / 4.63% | neutral |
| `hset@p16` | 1.069 | 6.17% / 4.45% | noisy |
| `hset@p128` | 1.175 | 6.02% / 7.02% | noisy |

Focused score: **1 win / 1 loss / 2 neutral / 5 noisy**. Clean cells only:
**1 win / 1 loss / 2 neutral**. The current clean frontier for this narrow gate is now `INCR@P1`;
`MIXED@P1` needs another quiet rerun before it is a valid code target.
