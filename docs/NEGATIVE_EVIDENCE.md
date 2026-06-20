# Negative Evidence Ledger

This file is the short-form evidence ledger requested for the 2026-06-20 cod-a
BOLD-VERIFY pass. The canonical long-form project ledger remains
`docs/perf_negative_evidence_ledger.md`.

## 2026-06-20 cod-a `frankenredis-ohsk5` pubsub direct encoder keep and pending-client rejection

Harness: per-crate release builds for `fr-server` and `fr-bench` through
`rch exec -- cargo build --release -p fr-server -p fr-bench`, with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`. Pubsub
fanout proof used saved pre-hunk FrankenRedis control binaries, the candidate
release binary, and vendored Redis 7.2.4. Metric is delivered subscriber-message
throughput.

Alien route: allocation-free hot-path serialization. The kept hunk bypasses
intermediate `RespFrame` construction for delivered pubsub messages and encodes
`message`, `pmessage`, `smessage`, and client-tracking `invalidate` pushes
directly into each connection's write buffer. A direct byte-equivalence unit test
pins RESP2 and RESP3 output against the existing frame encoder.

| artifact | variant | topology | median ratio | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T1823Z-pubsub-pending-vec-candidate/candidate_control_pubsub_fanout_32x4000_v2.txt` | pending-client `HashSet` to `Vec` candidate vs current-control | 32 subscribers, 4000 messages, pipe 32, trials 7 | 0.9963 candidate/control | rejected; no material gain |
| same | rejected pending-client candidate vs Redis 7.2.4 | same | 0.9575 candidate/redis; 0.9610 control/redis | no gap closure |
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T1823Z-pubsub-pending-vec-candidate/direct_encoder_pubsub_fanout_32x4000.txt` | direct pubsub encoder candidate vs current-control | 32 subscribers, 4000 messages, pipe 32, trials 7 | 1.0614 candidate/control | keep; primary gate |
| same | direct pubsub encoder candidate vs Redis 7.2.4 | same | 0.9967 candidate/redis; 0.9390 control/redis | nearly closes primary Redis gap |
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T1823Z-pubsub-pending-vec-candidate/direct_encoder_pubsub_fanout_32x4000_confirm.txt` | direct pubsub encoder confirmation | 32 subscribers, 4000 messages, pipe 32, trials 5 | 1.0150 candidate/control; 0.9411 candidate/redis | confirmed modest same-control win; Redis gap remains |
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T1823Z-pubsub-pending-vec-candidate/direct_encoder_pubsub_fanout_64x3000_confirm.txt` | direct pubsub encoder confirmation | 64 subscribers, 3000 messages, pipe 32, trials 5 | 1.0242 candidate/control; 0.9770 candidate/redis | confirmed modest same-control win; gap narrowed |

Discarded harness note: the first
`candidate_control_pubsub_fanout_32x4000.txt` run used a byte-by-byte subscriber
parser and failed delivery-completeness checks. It is retained as failed harness
evidence only; the buffered-parser v2 artifact is the valid rejection gate.

Crate-bench note: the literal requested `cargo bench --release -p fr-bench`
failed because this Cargo toolchain rejects `--release` for `cargo bench`.
The valid bench-profile command, `cargo bench -p fr-bench`, passed via `rch`
after building `fr-server` on the same remote worker and pinning `FR_SERVER_BIN`.
The broad crate bench is not the pubsub keep gate; it is recorded in the artifact
summary as crate-level smoke/context.

Correctness/guard evidence: `cargo fmt --check -p fr-command -p fr-server`,
`cargo check -p fr-command -p fr-server --all-targets`,
`cargo test -p fr-command direct_pubsub_encoder_matches_frame_encoder_bytes --
--nocapture`, `cargo clippy -p fr-command -p fr-server --all-targets -- -D
warnings`, and `cargo test -p fr-conformance -- --nocapture` all passed. The
conformance run completed with the usual non-strict replication live-oracle
replid/offset mismatches printed as non-asserting diagnostics, and the Rust test
suite exited 0.

Decision: keep the direct encoder and revert the pending-client `Vec` hunk. This
is a measured pubsub fanout improvement, but not full domination: confirmations
still show `0.9411x` and `0.9770x` Redis-relative medians, so pubsub remains a
release-readiness watch area.

## 2026-06-20 cod-b `frankenredis-ohsk5` cached write-gate extension rejection

Harness: per-crate release builds for `fr-server` and `fr-bench` through
`rch exec -- cargo build --release -p fr-server -p fr-bench`. The requested
shared target dir `/data/projects/.rch-targets/frankenredis-cod-b` had stale
nightly artifacts after an `rch` fallback, so the measured builds used fresh
cod-b-suffixed target dirs without deleting anything:
`frankenredis-cod-b-current-20260620T1139Z` for current-control and
`frankenredis-cod-b-cached-gate-candidate-20260620T1147Z` for the candidate.
Redis-relative rows used vendored Redis 7.2.4 `redis-benchmark`, P16, c50,
n150k, trials=7.

Candidate: extend the existing per-buffered-batch borrowed write-gate cache from
SET/HSET/MSET exact packets to SADD/LPUSH/RPUSH and flagless ZADD exact packet
fast paths. This targeted the shared conservative gate scan in the residual
write cluster without changing store layout or generic fallback behavior.
`cargo fmt --package fr-server --package fr-runtime -- --check`,
`cargo check -p fr-server --all-targets`, and
`cargo check -p fr-runtime --all-targets` passed via `rch` while the candidate
was applied.

Profiling note: a manual `perf record` attempt against ZADD was blocked by the
host kernel (`perf_event_paranoid=4`). The zero-sized data file and stderr are
recorded under
`artifacts/optimization/frankenredis-ohsk5-codb-sadd-zadd/20260620T1141Z-profile-zadd/`.
No synthetic profile claim is made.

| artifact | variant | command | ratio | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-ohsk5-codb-sadd-zadd/20260620T1140Z-current/current_vs_redis.txt` | current-control vs Redis 7.2.4 | lpush/rpush/sadd/zadd | 0.6854 / 0.7895 / 0.8284 / 0.7824 | residual write losses confirmed |
| same | current-control vs Redis 7.2.4 | set/get/hset/incr | 0.99 / 0.98 / 1.07 / 0.99 | scalar/read guards at parity or better |
| `artifacts/optimization/frankenredis-ohsk5-codb-sadd-zadd/20260620T1149Z-candidate-control/candidate_vs_control.txt` | cached gate candidate vs current-control | lpush/rpush/sadd/zadd | 0.96 / 1.01 / 1.02 / 1.03 | rejected; noise-scale and LPUSH soft down |
| same | cached gate candidate vs current-control | set/get/hset/incr | 1.01 / 1.03 / 1.01 / 1.06 | guard neutral/noisy |
| `artifacts/optimization/frankenredis-ohsk5-codb-sadd-zadd/20260620T1150Z-candidate-redis/candidate_vs_redis.txt` | rejected candidate vs Redis 7.2.4 | lpush/rpush/sadd/zadd | 0.6608 / 0.8041 / 0.8571 / 0.7740 | release gaps remain |
| same | rejected candidate vs Redis 7.2.4 | set/get/hset/incr | 1.03 / 1.00 / 1.01 / 1.02 | non-target guards remain fine |

Decision: reject and revert the runtime/server source hunk before commit. The
candidate did not materially move SADD/ZADD and made the biggest LPUSH gap
slightly worse in the same-current gate. Do not retry borrowed write-gate cache
extension as a standalone lever; the remaining list/set/zset write losses need a
larger mutation/storage or parser-ordering primitive with fresh proof. Final
reverted-source conformance passed via `rch exec -- cargo test -p fr-conformance
-- --nocapture`.

## 2026-06-20 cod-b `frankenredis-ohsk5` packed-list direct prepend

Harness: per-crate release builds for `fr-server` and `fr-bench` through
`rch exec -- cargo build --release -p fr-server -p fr-bench`, with isolated
target dirs under `/data/projects/.rch-targets/frankenredis-cod-b-lpush-*`.
Candidate/control and Redis-relative rows used vendored Redis 7.2.4
`redis-benchmark`, P16, c50, n150k, trials=7 against fresh servers.

Candidate: replace `PackedList::push_front`'s temporary encoded `Vec` plus
`Vec::splice(0..0, enc)` with a direct reserve/resize/copy-within prepend. This
kept the same packed byte layout and passed `cargo check -p fr-store --all-targets`,
the `list_equivalent_to_vecdeque` focused property test, and touched-file
`rustfmt --edition 2024 --check`, but did not produce a keepable LPUSH win.

| artifact | variant | command | ratio | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-ohsk5-packedlist-prepend-codb/20260620T111500Z/control_vs_redis.txt` | current-control vs Redis 7.2.4 | lpush/rpush/sadd/zadd/set/get/hset/incr | 0.7548 / 0.8371 / 0.8162 / 0.8204 / 1.0204 / 1.0321 / 1.0696 / 1.0261 | residual write losses remain |
| `artifacts/optimization/frankenredis-ohsk5-packedlist-prepend-codb/20260620T112000Z/candidate_control.txt` | direct prepend candidate vs current-control | lpush | 0.9784 | rejected, no material gain |
| same | direct prepend candidate vs current-control | rpush/sadd/zadd/set/get/hset/incr | 1.0374 / 1.0061 / 1.0208 / 1.0000 / 1.0268 / 0.9936 / 0.9290 | mixed/noisy guards; code path only targeted LPUSH |
| `artifacts/optimization/frankenredis-ohsk5-packedlist-prepend-codb/20260620T112000Z/candidate_vs_redis.txt` | rejected candidate vs Redis 7.2.4 | lpush/rpush/sadd/zadd/set/get/hset/incr | 0.7435 / 0.9106 / 0.9006 / 0.8058 / 1.0280 / 1.0657 / 1.0135 / 0.9866 | LPUSH and ZADD still losses |

Decision: reject and revert the `PackedList::push_front` hunk before commit. The
allocation-free front prepend did not close the LPUSH gap; the measured list
write problem is deeper than `Vec::splice`'s temporary allocation. Do not retry
this standalone packed-list direct-prepend micro-lever. Next list-write attempts
need a larger storage representation change, a batch-aware list push primitive,
or fresh profile evidence that names a different LPUSH/RPUSH hotspot.

## 2026-06-20 cod-a `frankenredis-ohsk5.65` front-biased list chunk keep

Harness: per-crate release builds for `fr-server` and `fr-bench` through
`rch exec -- cargo build --release -p fr-server -p fr-bench`, with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`. Redis-relative
rows used vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n200k, seven
trials through `scripts/bench_vs_redis.py`. Direct candidate/control rows used
the same `redis-benchmark` client against simultaneously resident control
(`19742`) and candidate (`19743`) FrankenRedis binaries.

Alien route: cache-aware deque/list layout rather than another threshold tweak.
The kept hunk makes an active front `ListChunk::Owned` store logical order
reversed, so repeated `LPUSH` uses `Vec::push` at the physical tail instead of
`Vec::insert(0, ...)` shifting the whole chunk. Iteration, reverse iteration,
random access, DUMP quicklist export, and arbitrary mutation normalize/translate
the representation back to logical order.

| artifact | variant | command | median ratio | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-ohsk5.65/20260620T1133Z/control_vs_redis_list_writes.txt` | current-control vs Redis 7.2.4 | lpush | 0.72 | confirmed loss |
| same | current-control vs Redis 7.2.4 | rpush | 0.81 | confirmed loss |
| same | current-control vs Redis 7.2.4 | sadd | 0.84 | confirmed loss/noisy |
| same | current-control vs Redis 7.2.4 | zadd | 0.78 | confirmed loss |
| `artifacts/optimization/frankenredis-ohsk5.65/20260620T1133Z/candidate_vs_redis_list_writes.txt` | candidate vs Redis 7.2.4 | lpush | 0.85 | win vs current, still below Redis |
| same | candidate vs Redis 7.2.4 | rpush | 0.89 | improved, still below Redis |
| same | candidate vs Redis 7.2.4 | sadd | 0.86 | neutral/residual loss |
| same | candidate vs Redis 7.2.4 | zadd | 0.74 | residual loss; direct A/B says no source regression |
| `artifacts/optimization/frankenredis-ohsk5.65/20260620T1133Z/candidate_vs_control_list_writes.txt` | candidate vs current-control | lpush | 1.104 | keep: direct A/B win |
| same | candidate vs current-control | rpush | 1.013 | neutral guard |
| same | candidate vs current-control | sadd | 1.027 | neutral guard |
| same | candidate vs current-control | zadd | 1.030 | neutral guard |
| `artifacts/optimization/frankenredis-ohsk5.65/20260620T1133Z/candidate_vs_control_lpush_confirm.txt` | focused confirmation vs current-control | lpush | 1.170 | confirmed keep |

Correctness/guard evidence: `rustfmt --edition 2024 --check
crates/fr-store/src/packed_set.rs`, `cargo check -p fr-store --all-targets`,
`cargo test -p fr-store list -- --nocapture`, `cargo clippy -p fr-store
--all-targets -- -D warnings`, and `cargo test -p fr-conformance --
--nocapture` all passed; the rustfmt check was local and the cargo gates ran via
`rch`. Live differential guards also passed: `scripts/list_differ.py --oracle 19741 --fr
19743 --iters 500 --seed 65065` and
`scripts/list_quicklist_dump_differ.py 19741 19743`.

Decision: keep the front-biased `ListChunk` layout. It does not fully close
LPUSH (`0.85x` vs Redis remains a release-readiness loss), but it is a measured
same-run LPUSH improvement with neutral guards. Next list work should continue
deeper into Redis-relative list-write residuals rather than repeating packed-list
promotion thresholds.

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

## 2026-06-20 cod-a `frankenredis-ohsk5.64` INCR/list-write pivot and LPUSH front-promotion rejection

Harness: vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n150k, seven
interleaved trials through `scripts/bench_vs_redis.py`. FrankenRedis release
binaries were built per crate through `rch exec -- cargo build --release -p
fr-server -p fr-bench` with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`. Current
control stayed resident on port 31992 while the candidate ran on 31993, so the
candidate/control gate isolated the source hunk from Redis-side variance.

Initial route: BlackThrush's inbox note suggested the `INCR` write-invalidation
path might still be a loss. The fresh current/Redis gate did not reproduce that
as the largest gap, so no cache-invalidation hunk was attempted.

| artifact | variant | command | median ratio | verdict |
|---|---|---|---:|---|
| `artifacts/optimization/frankenredis-ohsk5.64/20260620T1057Z/current_vs_redis_incr_write_guard.txt` | current vs Redis 7.2.4 | incr | 0.98 | neutral; no INCR cache-guard source attempt |
| same | current vs Redis 7.2.4 | set | 0.99 | neutral |
| same | current vs Redis 7.2.4 | sadd | 0.90 | parity-floor loss/noisy edge |
| same | current vs Redis 7.2.4 | lpush | 0.72 | confirmed loss; pivot target |
| same | current vs Redis 7.2.4 | rpush | 0.82 | confirmed loss |
| same | current vs Redis 7.2.4 | zadd | 0.75 | confirmed loss |
| `artifacts/optimization/frankenredis-ohsk5.64/20260620T1057Z/candidate_vs_current_list_front_promote.txt` | early `LPUSH` packed-list front promotion vs current-control | lpush | 0.95 | rejected; no win |
| same | early `LPUSH` packed-list front promotion vs current-control | rpush/sadd/zadd/incr/set | 1.05 / 1.03 / 0.97 / 1.01 / 0.99 | noise-scale guard cells |
| `artifacts/optimization/frankenredis-ohsk5.64/20260620T1057Z/candidate_vs_redis_list_front_promote.txt` | early `LPUSH` packed-list front promotion vs Redis 7.2.4 | lpush | 0.73 | still a loss |
| same | early `LPUSH` packed-list front promotion vs Redis 7.2.4 | rpush/sadd/zadd/incr/set | 0.90 / 0.90 / 0.78 / 1.04 / 1.08 | residual list/zset losses; scalar writes fine |

Guard runs before rejection: `cargo test -p fr-store --lib
list_value_deque_equivalent_to_vecdeque_after_promotion`, `cargo test -p
fr-store --lib list_value_cow_mutations_preserve_independent_order`, and `cargo
check -p fr-store --all-targets` all passed via `rch`. Final reverted-source
conformance guard also passed via `rch exec -- cargo test -p fr-conformance --
--nocapture`. Correctness was not the rejection reason.

Decision: revert/not ship the early front-promotion hunk in
`crates/fr-store/src/packed_set.rs`. It did not close the measured LPUSH gap and
was slightly worse than the saved current-control. Do not retry "promote packed
lists earlier on front insert" as a standalone lever unless a fresh profile
names `PackedList::push_front` byte shifting on a workload larger than this
P16/c50 benchmark. The next list-write route should target the actual mutation
primitive: chunk/front-fill layout, command-path batching, or a quicklist-style
node builder that avoids per-element packed front shifts without sacrificing the
small-list locality that this rejected hunk disturbed.

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

## 2026-06-20 cod-b `frankenredis-gu5nf` ZCOUNT compact-slice count rejection

Harness: `scripts/broad_command_headtohead.py`, vendored Redis 7.2.4, `--pipe
200 --trials 9`, plus one focused `ZCOUNT` candidate/control run at `PIPE=5000`
and 21 trials. Release binaries were built with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b` through
`rch exec -- cargo build --release -p fr-server -p fr-bench`; the isolated
candidate came from detached worktree
`/data/projects/.worktrees/frankenredis-cod-b-zcount-20260620T133708Z` at
`8f7192689` with only the compact full-zset count hunk applied.

Binary fingerprints:

| binary | sha256 |
|---|---|
| control `frankenredis` | `28bfaadf5f4abf0ab07d784572d16fdc8f8bfc5e4724719fb18ea92f70e4991f` |
| candidate `frankenredis` | `32dfc7e30ef2d4791cd721724050dab9f29aa788731cc9b3b724949ab62e8d2a` |
| Redis 7.2.4 server | `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7` |

Idea tested: for compact full zsets, `FullZSetOrder::range` already binary
searches score bounds and returns a contiguous slice. The candidate replaced
the cold `ZCOUNT` slice walk with `window.len()` when all entries were actual
members, falling back to the existing sentinel-filtering scan if corrupted
test sentinels were present.

| gate | command | fr/Redis 7.2.4 or candidate/control ratio | verdict |
|---|---|---:|---|
| control vs Redis | `getrange` | 0.85 | loss |
| control vs Redis | `bitcount` | 2.12 | win |
| control vs Redis | `sintercard` | 0.77 | loss |
| control vs Redis | `sinterstore` | 0.96 | neutral |
| control vs Redis | `sunionstore` | 0.99 | neutral |
| control vs Redis | `sdiffstore` | 0.92 | neutral |
| control vs Redis | `sinter3` | 0.90 | neutral |
| control vs Redis | `smismember` | 0.74 | loss |
| control vs Redis | `zrangebyscore` | 1.02 | neutral |
| control vs Redis | `zrange_rev` | 0.92 | neutral |
| control vs Redis | `hrandfield` | 1.10 | win |
| control vs Redis | `zrandmember` | 1.15 | win |
| control vs Redis | `srandmember` | 1.08 | win |
| control vs Redis | `lrange_full` | 1.01 | neutral |
| control vs Redis | `lpos` | 2.10 | win |
| control vs Redis | `zcount` | 0.63 | target loss confirmed |
| candidate vs control, broad | `zcount` | 1.03 | neutral, below keep threshold |
| candidate vs control, focused | `zcount` | 0.982 | rejected; candidate slower |
| candidate vs Redis | `getrange` | 0.68 | loss/noise guard |
| candidate vs Redis | `bitcount` | 2.15 | win |
| candidate vs Redis | `sintercard` | 0.66 | loss |
| candidate vs Redis | `sinterstore` | 0.97 | neutral |
| candidate vs Redis | `sunionstore` | 0.99 | neutral |
| candidate vs Redis | `sdiffstore` | 1.04 | neutral |
| candidate vs Redis | `sinter3` | 0.92 | neutral |
| candidate vs Redis | `smismember` | 0.99 | neutral |
| candidate vs Redis | `zrangebyscore` | 0.99 | neutral |
| candidate vs Redis | `zrange_rev` | 0.92 | neutral |
| candidate vs Redis | `hrandfield` | 1.06 | win |
| candidate vs Redis | `zrandmember` | 1.08 | win |
| candidate vs Redis | `srandmember` | 0.93 | neutral |
| candidate vs Redis | `lrange_full` | 1.04 | neutral |
| candidate vs Redis | `lpos` | 2.75 | win |
| candidate vs Redis | `zcount` | 0.65 | loss, unchanged frontier |

Correctness guard: the isolated candidate passed
`cargo test -p fr-store score_bound_count -- --nocapture`, including the new
compact full-zset sentinel fallback test and the existing warm-treap
isomorphism test. `rch` timed out during that test sync and ran locally; the
release build later succeeded remotely on `vmi1149989`. Final source
conformance after reverting the candidate passed via
`rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo test -p fr-conformance -- --nocapture`
on `hz2` (`194` library tests plus conformance binaries, smoke, live, and
doc-test suites green).

Artifacts:
`artifacts/optimization/frankenredis-codb-zcount-compact-count/20260620T133708Z/`
contains the control/candidate binaries, the candidate patch, control-vs-Redis,
candidate-vs-control, focused `ZCOUNT`, and candidate-vs-Redis outputs.

Decision: reject and revert the compact-slice `ZCOUNT` count hunk. A colder
`window.len()` shortcut does not beat the existing slice scan once measured at
higher repetition, and Redis-relative `ZCOUNT` remains a loss (`0.65x` in the
candidate gate, `0.63x` baseline). Do not retry this exact compact-count lever
without a fresh profile proving the scan/filter itself dominates; route deeper
to zset representation/rank-index parity or broader command dispatch overhead.

## 2026-06-20 cod-a bold-verify current refresh + rejected borrowed ZADD no-op shortcut

Harness: vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n150k, interleaved
trials through `scripts/bench_vs_redis.py`. FrankenRedis release builds used
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a` via
`rch exec -- cargo build --release -p fr-server`. Servers reported
`connected_slaves=0` before measurement. This pass was a fresh restart under
agent `CobaltCove`.

Current refresh before the attempted ZADD lever:

| artifact | command | median fr/redis | verdict |
|---|---|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T133457Z/current_vs_redis_standard_p16_c50_n150k_trials7.txt` | set | 0.98x | neutral |
| same | get | 1.01x | neutral/win |
| same | incr | 0.98x | neutral |
| same | lpush | 0.79x | loss |
| same | rpush | 0.74x | loss |
| same | lpop | 1.06x | win |
| same | rpop | 1.16x | win |
| same | sadd | 0.81x | loss |
| same | hset | 1.01x | neutral/win |
| same | spop | 1.01x | neutral/win |
| same | zadd | 0.77x | loss |
| same | lrange_100 | 1.00x | neutral |
| same | mset | 0.93x | neutral |

Attempted lever: parsed `ZADD key score member ...` into borrowed member slices
and added a store fast path that skipped owned member buffers for existing
members whose canonical score was unchanged. The idea was rejected and reverted:
the release benchmark stayed below Redis and worsened the target cell versus
the pre-edit refresh.

| artifact | command | median fr/redis | verdict |
|---|---|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T134553Z-zadd-borrowed-candidate/candidate_vs_redis_standard_p16_c50_n150k_trials9_zadd_family.txt` | zadd | 0.74x | rejected; worse than 0.77x refresh |
| same | sadd | 0.87x | residual loss; guard only |
| same | lpush | 0.94x | guard neutral, likely load/noise vs prior 0.79x |
| same | rpush | 0.90x | guard neutral |
| same | set | 1.09x | guard win |
| same | get | 1.00x | guard neutral |
| same | incr | 1.06x | guard win |
| same | hset | 1.17x | guard win |

Decision: no ZADD source hunk remains from this experiment. Do not retry the
same "borrow existing member/no-op score" fast path without a profile proving
owned member materialization is the dominant cost. The live frontier from the
fresh refresh remains list writes (`LPUSH`/`RPUSH`), `SADD`, and deeper `ZADD`
storage/index work rather than parser-side no-op shortcuts.

## 2026-06-20 cod-a rejected list LP-byte reuse plumbing

Harness: vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n150k, 9 interleaved
trials, fresh Redis/frankenredis processes with `connected_slaves=0`. Release
builds used
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a rch exec -- cargo build --release -p fr-server`.

Candidate idea: reuse the `list_lp_entry_bytes(elem)` value already computed by
`ListValue::add_entry_bytes` and pass it into `ChunkedList` append/prepend so the
large-list path does not run the canonical integer/listpack sizing probe twice
for a pushed element.

Profiling note: local kernel profiling was blocked by
`kernel.perf_event_paranoid = 4`; `perf stat -e cycles:u,instructions:u -- sleep 0.1`
failed with the kernel access-denied message. The existing profiling helper was
not run because it deletes temp files during setup, which is forbidden in this
checkout. This pass therefore uses code inspection plus same-window release
A/B and Redis-relative measurement.

| artifact | command | candidate fr/redis | control fr/redis | candidate/control | verdict |
|---|---|---:|---:|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T141103Z-list-lpbytes-candidate/` | lpush | 0.92x | 0.93x | 0.99x | neutral/rejected |
| same | rpush | 0.82x | 0.87x | 0.94x | loss/rejected |
| same | lpop | 1.16x | 1.15x | 1.01x | neutral guard |
| same | rpop | 1.15x | 1.25x | 0.92x | guard down |
| same | lrange_100 | 1.06x | 1.05x | 1.01x | neutral guard |
| same | sadd | 0.85x | 0.83x | 1.02x | neutral guard; still below Redis |
| same | zadd | 0.75x | 0.77x | 0.97x | guard down; still below Redis |
| same | set | 1.07x | 1.09x | 0.98x | neutral guard |
| same | get | 1.00x | 1.01x | 0.99x | neutral guard |
| same | incr | 1.03x | 1.03x | 1.00x | neutral guard |
| same | hset | 1.13x | 1.16x | 0.97x | guard down |
| same | mset | 1.19x | 1.18x | 1.01x | neutral guard |

Decision: reject and keep no production hunk. Same-window control tied or beat
the candidate on the list-write targets, especially `RPUSH` (`0.87x` control vs
`0.82x` candidate). Do not retry this standalone LP-byte plumbing patch without
a profile proving the second sizing probe dominates. The measured frontier stays
`RPUSH`, `SADD`, and `ZADD` storage/index or batch-path work.

## 2026-06-20 cod-b rejected SMISMEMBER direct reply encoding

Harness: vendored Redis 7.2.4 plus saved FrankenRedis control binary, same host
ports, `scripts/broad_command_headtohead.py`, release builds through
`AGENT_NAME=CobaltCove rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo build --release -p fr-server -p fr-bench`.
The control binary SHA256 was
`9ae333a67212c1d5d7275a62b8c2e3c2fba7bbd0c3fc53ed7d1f0cf3e5c015c8`; the
candidate binary SHA256 was
`d636b9021c947de32b2adfedc8d62049188dceaf5d1f0ac9a6616c80aa33c1ca`.

Candidate idea: add `execute_plain_smismember_borrowed_into`, mirroring the
existing `ZMSCORE` direct encoder, so the network fast path writes the integer
array directly into `conn.write_buf` instead of allocating one `RespFrame` per
returned flag. This followed the alien/optimization pass as a branch-elision and
reply-materialization lever on the current `SMISMEMBER` loss cell.

Profiling note: local hardware-counter profiling was blocked by
`kernel.perf_event_paranoid = 4`; see
`artifacts/optimization/frankenredis-codb-smismember-sintercard-getrange/20260620T140406Z/perf_event_paranoid_block.txt`.
This decision therefore uses same-run release A/B timing.

| artifact | command | ratio vs Redis 7.2.4 | candidate/control | verdict |
|---|---|---:|---:|---|
| `artifacts/optimization/frankenredis-codb-smismember-sintercard-getrange/20260620T140406Z/control_vs_redis_broad.txt` | `smismember` control broad | 0.79x | n/a | baseline loss |
| same | `sintercard` control broad | 0.62x | n/a | baseline loss; not addressed |
| same | `zcount` control broad | 0.61x | n/a | baseline loss; prior compact-count lever already rejected |
| `.../candidate_vs_control_broad.txt` | `smismember` broad | n/a | 1.03x | neutral, not enough to keep |
| `.../candidate_vs_control_smismember_focused.txt` | `smismember` focused, pipe=2000 trials=21 | n/a | 0.96x | loss/rejected |
| `.../candidate_vs_redis_smismember_focused.txt` | `smismember` candidate focused | 0.99x | n/a | neutral vs Redis, failed same-run A/B |
| `.../control_vs_redis_smismember_focused.txt` | `smismember` control focused | 0.93x | n/a | focused control still below Redis |

Decision: reject and keep no production hunk. The exact same-run focused A/B is
the controlling evidence: the direct encoder was slower than the saved control
(`0.96x`). Do not retry `SMISMEMBER` reply-frame elimination alone; the next
route should attack set membership/storage layout, hash probing, or `SINTERCARD`
no-LIMIT set-intersection cost rather than only socket-buffer encoding.
