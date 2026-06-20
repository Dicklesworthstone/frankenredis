# Negative Evidence Ledger

This file is the short-form evidence ledger requested for the 2026-06-20 cod-a
BOLD-VERIFY pass. The canonical long-form project ledger remains
`docs/perf_negative_evidence_ledger.md`.

## 2026-06-20 cod-b `frankenredis-ohsk5` INCR store-probe consolidation

Harness: per-crate release builds for `fr-server` and `fr-bench` through
`rch exec -- cargo build --release -p fr-server -p fr-bench`, with isolated
target dirs under `/data/projects/.rch-targets/frankenredis-cod-b-*`.
Candidate/control A/B used `fr-bench`, P16, c50, n300k, trials=7 against fresh
FrankenRedis servers. Redis-relative rows used vendored Redis 7.2.4
`redis-benchmark`, P16, c50, n150k, trials=7 through `scripts/bench_vs_redis.py`.

| artifact | variant | command | ratio | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-ohsk5-incr-store-probe-codb/20260620T105145Z/summary.md` | candidate vs current-control | incr | 0.9886 | rejected, neutral |
| same | candidate vs current-control | set | 0.9377 | regression |
| same | candidate vs current-control | get | 0.9558 | regression/noisy |
| same | candidate vs current-control | hset | 0.8146 | regression/noisy |
| `artifacts/optimization/frankenredis-ohsk5-incr-store-probe-codb/20260620T105145Z/candidate_vs_redis.txt` | rejected candidate vs Redis 7.2.4 | incr/set/get/hset/lpush/rpush/sadd/zadd | 0.78 / 1.57 / 0.66 / 1.85 / 0.75 / 0.78 / 0.91 / 0.74 | mixed; candidate did not improve target |
| `artifacts/optimization/frankenredis-ohsk5-incr-store-probe-codb/20260620T105145Z/control_vs_redis.txt` | current-control vs Redis 7.2.4 | incr/set/get/hset/lpush/rpush/sadd/zadd | 0.94 / 1.04 / 1.00 / 1.06 / 0.71 / 0.81 / 0.87 / 0.79 | current residuals are list/set/zset writes |

Decision: the INCR candidate collapsed `drop_if_expired` + `key_has_expiry` into
a single expiry probe before the mutable entry lookup, duplicating the expired-key
side effects. Correctness-focused `fr-store incr` tests and `cargo check -p
fr-store --all-targets` passed, but the measured A/B did not pay and softened
guard workloads. The source hunk was reverted before commit. Do not retry this
standalone INCR expiry-probe consolidation; the open measured losses are still
`lpush`, `rpush`, `sadd`, and `zadd`, with `incr` near the parity floor on current
control.

## 2026-06-20 cod-b `frankenredis-ohsk5` non-store GET probes

Harness: vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n150k, interleaved
trials through `scripts/bench_vs_redis.py`. Builds were per-crate release builds
through `rch exec -- cargo build --release -p fr-server -p fr-bench` with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b` or an isolated
candidate target dir. Shared `fr-store` was reserved by BlackThrush, so this pass
only tested non-store server/runtime levers.

| artifact | variant | command | median ratio | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T061610Z-redis-benchmark-current/current_vs_redis_redis_benchmark.txt` | current vs Redis 7.2.4 | get | 0.83 | loss |
| same | current vs Redis 7.2.4 | lpush | 0.84 | loss, store/list-write lane |
| same | current vs Redis 7.2.4 | rpush | 0.74 | loss, store/list-write lane |
| same | current vs Redis 7.2.4 | sadd | 0.73 | loss, store/set lane |
| same | current vs Redis 7.2.4 | zadd | 0.69 | loss, store/zset lane |
| same | current vs Redis 7.2.4 | set/incr/hset/mset/lpop/rpop/spop | 0.99-1.24 | mixed neutral/wins; exact ratios in artifact |
| `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T061925Z-resp3-cache-candidate/candidate_vs_control_get_guard_20260620T0626Z.txt` | batch-local RESP3 cache vs current-control | get | 1.02 | rejected, noise-scale |
| same | batch-local RESP3 cache vs current-control | set/incr/hset/mset | 1.01 / 0.95 / 0.98 / 1.02 | guard neutral; `incr` soft loss |
| `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T0630Z-get-expire-count-gate/candidate_vs_control_get_guard_20260620T0632Z.txt` | skip GET fast active-expire call when no expiring keys vs current-control | get | 1.01 | rejected, noise-scale |
| same | skip GET fast active-expire call when no expiring keys vs current-control | set/incr/hset/mset | 0.99 / 0.97 / 0.95 / 1.01 | guard neutral-to-soft-loss |

Decision: both non-store GET candidates were reverted/not applied to shared
source. A 1-2% candidate/control median is not enough to close the measured
0.83x Redis-relative GET loss, and the guard cells were not directionally clean.
Do not retry session RESP3 caching or no-expire active-cycle elision as standalone
GET levers unless a fresh profile names them with low-variance timing. The
biggest confirmed losses in this pass remain store-owned list/set/zset writes,
plus BlackThrush's separate DUMP zset-listpack re-encode gap.

## 2026-06-20 cod-a `frankenredis-zset-listpack-score-zero-copy-z56kl` zset DUMP score fast path

Harness: custom `fr-bench --workload dump`, 50 clients, pipeline 128, keyspace
10000, vendored Redis 7.2.4 `redis-server`. Release binaries were built via
`rch` with `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`.

Profile route: BlackThrush's shared `dump@p128` profile named `lzf`,
`Store::dump_key`, and listpack score-entry encode/reparse work. Local kernel
`perf` was blocked in this pass by `perf_event_paranoid=4`, and the generic
`scripts/profile_hot_path.sh` path is not suitable for this workload because it
drives `redis-benchmark`, not the custom zset-prefilled `fr-bench dump` workload.

| artifact | variant | ratio | cv | verdict |
|---|---|---:|---|---|
| `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T061700Z-baseline/summary.txt` | current/control vs Redis | 0.616569 fr/redis | redis 5.27%, fr 3.13% | routing loss; Redis side slightly noisy |
| `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T062635Z-dirty-candidate-ab/summary.txt` | dirty integer-score fast path vs saved control | 1.080504 candidate/control | control 4.73%, candidate 4.96% | supporting win, not enough alone |
| same | dirty integer-score fast path vs Redis | 0.569797 candidate/redis | redis 16.78% | Redis leg too noisy; not a keep claim |
| `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T062741Z-candidate-control-confirm/summary.txt` | dirty integer-score fast path vs saved control, 500k requests, 9 trials | 0.955895 candidate/control | control 3.71%, candidate 2.38% | **rejected current form** |

Guard run:
`AGENT_NAME=cod-a CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a rch exec -- cargo test -p fr-store zset_score_int_listpack_fastpath_is_byte_identical_to_string_form -- --nocapture`
passed. Correctness was not the rejection reason.

Decision: do not keep or extend this score-integer shortcut from the current
mixed evidence. The stronger low-CV confirmation regressed throughput by 4.4%
against the saved pre-fastpath control. The dirty `fr-store` source was under
BlackThrush's active reservation, so cod-a recorded evidence only and did not
stage, commit, or revert that peer-owned hunk. Retry only with an isolated
retained-listpack or cached-DUMP representation that avoids rebuilding the whole
compact zset listpack, then prove it with same-current A/B before Redis claims.

## 2026-06-20 cod-a `frankenredis-15lug.1` SPOP parser ordering

Harness: vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n150k, interleaved
trials through `scripts/bench_vs_redis.py`. FrankenRedis release builds used
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a` via `rch`
except for saved comparator binaries under the artifact directory.

| artifact | variant | command | median fr/redis | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-15lug-1/20260620T053608Z-baseline/bench_vs_redis_p16_c50_n150k_trials7.txt` | baseline | spop | 0.75 | loss |
| same | baseline | lpush | 0.78 | loss |
| same | baseline | rpush | 0.91 | neutral |
| `artifacts/optimization/frankenredis-15lug-1/20260620T053837Z-spop-exact-parser-candidate/bench_vs_redis_p16_c50_n150k_trials7.txt` | exact SPOP parser only | spop | 0.86 | improved, still below 0.9x |
| same | exact SPOP parser only | lpush | 0.78 | loss |
| same | exact SPOP parser only | rpush | 0.91 | neutral |
| `artifacts/optimization/frankenredis-15lug-1/20260620T054137Z-control-candidate-ab/summary.txt` | control 1 | spop | 0.75 | loss |
| same | candidate 2 | spop | 0.83 | improved, still below 0.9x |
| same | candidate 3 | spop | 0.93 | win vs parity floor |
| same | control 5 | spop | 0.68 | loss |
| `artifacts/optimization/frankenredis-15lug-1/20260620T054808Z-early-keyed-pop-candidate/bench_vs_redis_p16_c50_n150k_trials7.txt` | exact SPOP parser plus early keyed-pop ordering | spop | 1.03 | win |
| same | exact SPOP parser plus early keyed-pop ordering | lpop | 1.02 | win |
| same | exact SPOP parser plus early keyed-pop ordering | rpop | 1.00 | neutral |
| same | exact SPOP parser plus early keyed-pop ordering | lpush | 0.75 | residual loss |
| same | exact SPOP parser plus early keyed-pop ordering | rpush | 0.91 | neutral |
| `artifacts/optimization/frankenredis-15lug-1/20260620T054843Z-early-keyed-pop-confirm/bench_vs_redis_p16_c50_n150k_trials7.txt` | confirmation | spop | 1.04 | confirmed win |
| same | confirmation | lpush | 0.78 | residual loss |
| same | confirmation | rpush | 0.89 | residual loss/noisy floor |

Invalid measurements: `control 4` and `control 4b` inside
`20260620T054137Z-control-candidate-ab` were discarded because Redis failed to
bind the chosen port; no throughput result from those launches was counted.

Profile evidence: `scripts/profile_hot_path.sh -t spop -P 16 -n 2000000 -c 50
-s 6 -r 100000` produced `/data/tmp/claude-1000/profile_hot_path_4149131.data`
and showed `process_buffered_frames` as the dominant server hotspot with failed
exact-parser probes ahead of keyed pop. That evidence routed the kept second
lever to parser ordering.

Decision: keep the no-count `SPOP key` exact keyed-pop parser and the early
keyed-pop ordering in `crates/fr-server/src/main.rs`. The original SPOP loss is
fixed in the focused Redis-relative gate. Do not retry SPOP parser reshuffling
unless a fresh profile names it again; the remaining measured gap is list push,
especially `LPUSH`.

## 2026-06-20 cod-b fresh-restart `frankenredis-15lug.1` SPOP verification

Harness: vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n150k, interleaved
trials through `scripts/bench_vs_redis.py`. FrankenRedis release builds used
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b` via `rch`.

| artifact | variant | command | median ratio | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T053450Z-baseline/current_vs_redis_redis_benchmark.txt` | current vs Redis | spop | 0.77 | confirmed loss |
| same | current vs Redis | lpush | 0.77 | residual loss |
| same | current vs Redis | rpush | 0.86 | residual loss |
| `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T054210Z-candidate-control/candidate_vs_control_redis_benchmark.txt` | exact SPOP packet only vs current-control | spop | 1.02 | too small |
| `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T054238Z-candidate-redis/candidate_vs_redis_redis_benchmark.txt` | exact SPOP packet only vs Redis | spop | 0.78 | rejected |
| `artifacts/optimization/frankenredis-15lug-spop-frontload-pop/20260620T055254Z-final-five-command/final_candidate_vs_control.txt` | final front-loaded keyed-pop vs current-control | spop | 1.25 | keep |
| same | final front-loaded keyed-pop vs current-control | lpop | 1.11 | keep guard |
| same | final front-loaded keyed-pop vs current-control | rpop | 1.08 | keep guard |
| same | final front-loaded keyed-pop vs current-control | lpush | 1.00 | no regression |
| same | final front-loaded keyed-pop vs current-control | rpush | 1.04 | no regression |
| `artifacts/optimization/frankenredis-15lug-spop-frontload-pop/20260620T055254Z-final-five-command/final_candidate_vs_redis.txt` | final front-loaded keyed-pop vs Redis | spop | 1.06 | SPOP floor cleared |
| same | final front-loaded keyed-pop vs Redis | lpop | 1.03 | parity/win |
| same | final front-loaded keyed-pop vs Redis | rpop | 1.01 | parity/win |
| same | final front-loaded keyed-pop vs Redis | lpush | 0.83 | residual loss, not candidate regression |
| same | final front-loaded keyed-pop vs Redis | rpush | 0.85 | residual loss, not candidate regression |
| `artifacts/optimization/frankenredis-15lug-spop-frontload-pop/20260620T055340Z-final-spop-focused/final_spop_candidate_vs_control.txt` | final SPOP-focused vs current-control, 11 trials | spop | 1.30 | confirmed keep |
| `artifacts/optimization/frankenredis-15lug-spop-frontload-pop/20260620T055340Z-final-spop-focused/final_spop_candidate_vs_redis.txt` | final SPOP-focused vs Redis, 11 trials | spop | 1.00 | confirmed parity |

Profile evidence:
`artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T054407Z-profile-current-spop/perf_report_no_children.txt`
sampled current/control SPOP and showed `process_buffered_frames` at 14.01%
self, `parse_command_args_borrowed_into` at 1.85%, `execute_plain_keyed_pop_borrowed`
at 1.71%, and `Store::spop` at only 0.38%. That routed the kept lever away
from set-storage work and toward parser ordering.

Decision: reject the exact-packet-only hunk because it left SPOP at 0.78x vs
Redis. Keep the front-loaded no-count keyed-pop parser ordering plus SPOP packet
recognition. LPUSH/RPUSH remain the next measured list-write gaps.
