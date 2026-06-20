# Negative Evidence Ledger

This file is the short-form evidence ledger requested for the 2026-06-20 cod-a
BOLD-VERIFY pass. The canonical long-form project ledger remains
`docs/perf_negative_evidence_ledger.md`.

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
