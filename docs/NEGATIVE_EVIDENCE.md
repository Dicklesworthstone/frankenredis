# Negative Evidence Ledger

This file is the short-form evidence ledger requested for the 2026-06-20 cod-a
BOLD-VERIFY pass. The canonical long-form project ledger remains
`docs/perf_negative_evidence_ledger.md`.

## 2026-06-21 cod-a `frankenredis-ohsk5` SADD single-member runtime path rejected

DISK-LOW carry-forward hunk tested and reverted. The candidate routed canonical
and generic borrowed single-member `SADD key member` packets to a fixed-shape
`Runtime::execute_plain_sadd_one_borrowed`, bypassing the shared variadic
`SADD`/`LPUSH`/`RPUSH` runtime plumbing. That was the right target from the
arity sweep (`SADD` was `0.73x` fr/Redis at arity 1 but `1.16x` at arity 8 and
`1.23x` at arity 16), but the isolated measurement did not pay enough.

Valid bench: `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a
rch exec -- cargo bench -p fr-bench --profile release --bench
keyed_write_vs_redis -- SADD_1v` on worker `vmi1227854`, after a per-crate
`fr-server` release build on the same target dir. The bench harness now includes
arity 1 in `keyed_write_vs_redis` so the filtered Criterion run exercises the
Redis-benchmark default single-member SADD shape directly.

| gate | Redis 7.2.4 | FrankenRedis candidate | fr/Redis | verdict |
|---|---:|---:|---:|---|
| `keyed_write_vs_redis/SADD_1v`, median throughput | `1.7901 Melem/s` | `1.3708 Melem/s` | `0.77x` | reject; still below 0.9 parity floor and only a noisy ~5% lift vs the prior `0.73x` routing baseline |

Discarded harness misuse: `rch exec -- bash -lc 'cargo build --release -p
fr-server && cargo bench ...'` did not run remotely; `rch` rejected the shell
wrapper as a non-compilation command and the local fallback hit stale target-dir
rustc metadata (`E0514`) before any benchmark executed. This is not performance
evidence.

Decision: revert the production `execute_plain_sadd_one_borrowed` helper and
server-side routing shim; keep only the benchmark harness arity-1 coverage and
this negative evidence. Do not retry single-member SADD runtime shape plumbing
without a same-window control and a clearer path above the Redis parity floor.

Post-revert validation: `cargo fmt --check --package fr-runtime --package
fr-server --package fr-bench`, RCH `cargo check -p fr-runtime -p fr-server
-p fr-bench --all-targets`, RCH `cargo clippy -p fr-runtime -p fr-server
-p fr-bench --all-targets -- -D warnings`, and RCH `cargo test -p
fr-conformance -- --nocapture` all passed. Targeted `ubs` on the changed file
set returned nonzero on existing broad inventories in the monolithic runtime and
server files plus bench-harness panic/TcpStream heuristics; its embedded fmt,
clippy, cargo-check, and test-build sections were clean.

## 2026-06-21 cod-b `frankenredis-uhthd` SDIFF secondary-source lookup measured keep

Code-only lever shipped in `7b94d4efc` for `sdiff_value`: secondary SDIFF
sources no longer pay an unconditional `contains_key` probe before `get_mut`
when LFU tracking is disabled. The LFU-enabled path keeps the existence
pre-check so it preserves the prior per-existing-key RNG draw sequence.

Measured gate: filtered per-crate Criterion bench
`cargo bench -p fr-bench --bench set_algebra_vs_redis -- SDIFFSTORE`, with
`RCH_WORKER=ovh-a`, `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`,
and current `fr-server` release binary
`sha256=44622477fd90e2c54dde633f454a8624af17b3e83a6d867c5145f70721625cb7`.

| gate | Redis 7.2.4 | FrankenRedis | ratio vs Redis | verdict |
|---|---:|---:|---:|---|
| `set_algebra_vs_redis/SDIFFSTORE`, Criterion mean time | `622,693 ns` | `303,346 ns` | `0.487x` time, `2.05x` throughput | keep; current fr is faster than Redis on this row |

Discarded harness attempts: two earlier `fr-bench` runs failed before measuring
because `cargo bench -p fr-bench` does not build `fr-server`, and `rch` rewrites
remote `CARGO_TARGET_DIR` unless `FR_SERVER_BIN` is passed inside the remote
`env`. They produced no performance evidence.

Validation: `AGENT_NAME=BlackThrush RCH_REQUIRE_REMOTE=1
CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec --
cargo test -p fr-conformance -- --nocapture` passed: 194 lib tests, all
`fr-conformance` bin tests, 99 smoke tests, and doctests green. Non-strict live
oracle drift was printed but not asserted, matching the existing harness mode.

## 2026-06-21 cod-b `frankenredis-uhthd` compact PackedZSet score tags rejected

Harness: clean HEAD control worktree `43f17ad91`, candidate with only the
temporary compact `PackedZSet` score-tag hunk plus the `fr-store` clippy cleanup,
per-crate `rch exec -- cargo build --release -p fr-server -p fr-bench`, and
fresh-process memory probes against vendored Redis 7.2.4. Artifact:
`artifacts/optimization/frankenredis-uhthd-packed-zset-score-codb/20260621T003043Z/`.

| gate | ratios vs Redis 7.2.4 | verdict |
|---|---|---|
| broad control memory | keyspace/string_1k/list/hash/set/zset/stream = 1.516 / 0.955 / 1.123 / 1.336 / 1.308 / 1.715 / 0.929 | current zset loss confirmed |
| broad candidate memory | keyspace/string_1k/list/hash/set/zset/stream = 1.728 / 0.972 / 1.312 / 1.367 / 1.443 / 1.595 / 0.970 | zset moves better, unrelated cells drift worse; not enough alone |
| focused packed-zset RSS control | 6,250 zsets x 32 integer-score members: Redis 4.59 MB, fr 7.19 MB = 1.57x | direct target baseline |
| focused packed-zset RSS candidate | 6,250 zsets x 32 integer-score members: Redis 4.58 MB, fr 7.25 MB = 1.58x | no target win; reject |

Decision: rejected and source reverted. The broad scorecard had one favorable
zset cell, but the direct packed-zset RSS probe did not confirm it and the
candidate broad run failed the memory ratchet on list. Do not retry score-byte
tagging as a memory lever; the remaining zset gap is dominated by deeper
per-key/per-member representation overhead.

## 2026-06-20 cod-a `frankenredis-ohsk5` SADD compact-map single-probe rejection

Harness: per-crate release builds for `fr-server`/`fr-bench` through
`rch exec -- cargo build --release -p fr-server -p fr-bench`, with
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a` for the
candidate and `/data/projects/.rch-targets/frankenredis-cod-a-control` for the
control. Redis-relative rows used vendored Redis 7.2.4 `redis-benchmark`, P16,
c50, n150k, keyspace 100k, best-of-7 unless noted. Candidate temporarily made
`CompactFieldMap::insert_borrowed` reuse the vacant slot found during the miss
probe, avoiding the second hash/probe pass for new SADD members. Source was
reverted after measurement; no production hunk shipped.

| gate | fr/Redis ratios | verdict |
|---|---|---|
| current baseline, best-of-5 | lpush/rpush/sadd/zadd/set/get/hset/incr = 0.83 / 0.87 / 0.67 / 1.54 / 1.22 / 1.23 / 1.21 / 0.98 | SADD largest current loss; ZADD already a win in this window |
| candidate vs Redis | sadd/lpush/rpush/zadd/set/get/hset/incr = 0.84 / 0.86 / 0.88 / 1.31 / 1.29 / 1.23 / 1.19 / 0.97 | Redis-relative SADD looked better, but Redis side was slower |
| reverted control vs Redis | sadd/lpush/rpush/zadd/set/get/hset/incr = 0.76 / 0.86 / 0.79 / 1.39 / 1.35 / 1.28 / 1.22 / 1.04 | same-window control for decision |
| candidate rerun vs Redis | sadd/lpush/rpush/zadd/set/get/hset/incr = 0.79 / 0.89 / 0.79 / 1.16 / 1.37 / 1.15 / 1.34 / 1.05 | confirms SADD still below parity floor |

Decision: reject and keep source reverted. Absolute target throughput did not
beat the same-window control: SADD candidate `663,716`/`666,666` req/s vs
control `681,818` req/s (`0.97x`/`0.98x` candidate/control). Guard commands were
mixed and noisy: first candidate/control qps movement was lpush/rpush/zadd/set/get/hset/incr
= `1.04 / 1.04 / 0.88 / 0.99 / 0.99 / 1.08 / 0.99`. Do not retry this
single-probe compact-map insertion as a standalone SADD lever; the residual
needs deeper set mutation/storage work or a profile-backed parser/batch path.

Validation while the candidate was applied: `cargo check -p fr-store
--all-targets`, `cargo test -p fr-store ideww -- --nocapture`, and `cargo test
-p fr-store generic_hash_set_inline_members_preserve_indexset_semantics --
--nocapture` passed via `rch`. The malformed multi-filter Cargo test command
failed before running tests (`unexpected argument 'compact_str_set'`) and is
discarded as harness misuse, not code evidence.

## 2026-06-20 cod-b `frankenredis-uhthd` current-control memory scorecard

Harness: clean detached worktree at `d568ff5f0`, minimized Redis oracle payload
for RCH transfer, fail-closed remote build
`RCH_REQUIRE_REMOTE=1 CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec -- cargo build --release -p fr-server -p fr-bench`
on `vmi1152480`, followed by fresh-process
`scripts/memory_baseline_capture.py` against vendored Redis 7.2.4 with
`FR_BENCH_PORT_BASE=45251`.

No source hunk shipped in this pass. The relevant store files were actively
reserved by CobaltCove (`crates/fr-store/src/lib.rs`,
`crates/fr-store/src/keyspace_dict.rs`, and later `crates/fr-store/src/packed_set.rs`),
so this is a measured routing/scorecard update, not a code-change claim.

| data type | fr/redis RSS | fr/redis used_memory | verdict |
|---|---:|---:|---|
| zset | 1.728 | 0.619 | largest current RSS loss |
| hash | 1.562 | 0.838 | loss |
| keyspace | 1.403 | 0.805 | `uhthd` loss remains |
| set | 1.303 | 0.562 | loss |
| list | 1.078 | 0.391 | small loss |
| stream | 0.978 | 1.096 | RSS win; modeled memory loss |
| string_1k | 0.903 | 0.964 | win |

Score: **2 wins / 5 losses / 0 neutral** vs Redis 7.2.4 on RSS. Ratchet
status: pass, no regressions versus the prior tracked baseline. The measured
next targets are zset/hash/keyspace layout, but do not retry the rejected
inline-small key or sparse sidecar modification-count families without new
A/B evidence.

RCH negative evidence: copying the full untracked Redis oracle into a detached
worktree made remote sync time out at 30s and fail closed under
`RCH_REQUIRE_REMOTE=1`; a minimized payload (`src/commands`, `redis-server`,
`redis-cli`) synced in 37.49s and produced the valid remote release build.

## 2026-06-20 cod-b `frankenredis-uhthd` compact tagged PackedZSet score evidence

Harness: per-crate release builds for `fr-server` and `fr-bench`, with the
cod-b target root `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`,
plus the memory baseline harness against vendored Redis 7.2.4. Artifact:
`artifacts/optimization/frankenredis-uhthd-packed-zset-score-codb/20260620T1915Z/`.

Measured candidate: encode exact integer `PackedZSet` scores as a compact tagged payload
(`i8`/`i16`/`i32`) and retain raw `f64` bytes for fractional, large, infinite,
and NaN scores. This targets the zset RSS gap where Redis listpack can store
common integer scores compactly while FrankenRedis previously used eight score
bytes for every packed zset member.

| gate | ratios vs Redis 7.2.4 | verdict |
|---|---|---|
| current-control memory | hash/keyspace/list/set/stream/string_1k/zset = 1.422 / 1.405 / 1.396 / 1.093 / 0.978 / 0.931 / 1.619 | zset target loss confirmed |
| rebuilt candidate memory | hash/keyspace/list/set/stream/string_1k/zset = 1.205 / 1.365 / 1.195 / 1.259 / 0.980 / 0.891 / 1.456 | keep for zset; residual zset loss remains |
| best candidate memory run | hash/keyspace/list/set/stream/string_1k/zset = 1.249 / 1.489 / 1.127 / 1.141 / 0.968 / 0.924 / 1.271 | supporting target win only |
| failed-ratchet rerun | keyspace/string/list/hash/set/zset/stream = 1.417 / 0.928 / 1.338 / 1.468 / 1.526 / 1.292 / 0.981 | negative evidence; do not claim non-target cells |
| ZADD throughput guard | median 0.93x candidate/Redis, trials 0.93 / 1.01 / 0.59 under loadavg 43.46 | above parity floor, noisy guard |

Correctness/guard evidence: packed-zset iteration preserves score/member sort
order and zero canonicalization; raw-f64 fallback preserves fractional and
non-finite score behavior. Validation recorded for RCH release build,
`cargo check -p fr-store --all-targets`, `cargo test -p fr-store zset --
--nocapture`, `cargo clippy -p fr-store --all-targets -- -D warnings`,
`cargo test -p fr-conformance -- --nocapture`, touched-file rustfmt, and
targeted `ubs`.

Decision: evidence supports keeping the compact score encoding once the
peer-owned source hunk lands. This narrows zset memory, but it is not domination:
final rebuilt zset RSS is still `1.456x` Redis and the broad memory score remains
2 wins / 5 losses / 0 neutral. Do not retry this byte-level score compaction for
non-integer-heavy zsets without fresh A/B proof; the next `uhthd` target should
be deeper zset/keyspace layout.

### cod-a recheck on the same shared hunk

Artifact:
`artifacts/optimization/frankenredis-bold-verify-coda/20260620T1609Z-packed-zset-coda-verify/`.
Per-crate cod-a gates passed: `rch exec -- env
CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo build
--release -p fr-server -p fr-bench`, `cargo check -p fr-store --all-targets`,
`cargo test -p fr-store zset -- --nocapture`, `cargo clippy -p fr-store
--all-targets -- -D warnings`, `cargo test -p fr-conformance -- --nocapture`
(RCH local fallback), and `cargo fmt -p fr-store --check`.

Read-only packed-zset RSS probe, fresh processes, 6,250 zsets x 32 members
(200,000 packed members): Redis data-RSS `4.58 MB`, FrankenRedis data-RSS
`8.11 MB`, ratio `1.77x` fr/Redis. Verdict: negative evidence for domination
and for broad memory readiness. The compact integer-score hunk still has
supporting target evidence from the cod-b run, but cod-a's fresh packed-zset
probe says the remaining representation gap is larger than the committed final
baseline cell; next work must remove deeper per-key/member overhead rather than
another score-byte tweak.

Read-only ZADD throughput guard on the same cod-a binary, Redis benchmark P16,
c50, n150k, trials5, loadavg `11.21`: median `0.77x` fr/Redis with trials
`0.77 / 0.64 / 0.79 / 0.82 / 0.74`. Verdict: negative evidence against using
the compact-score hunk as a throughput/readiness claim; ZADD remains below the
`0.9x` parity floor in this recheck.

Targeted `ubs` on `crates/fr-store/src/packed_set.rs` returned nonzero on
file-wide legacy/static-analysis findings, including false-positive JWT
`decode` hits on existing `cfm_decode` helpers plus existing unwrap/clone/index
inventories. No new compiler, clippy, fmt, zset, or conformance failures were
introduced by the verified hunk.

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

## 2026-06-20 cod-a kept fr-persist presorted zset RDB fast path; DUMP/reload remain Redis losses

Harness notes:

- Primary requested RCH release build in a clean detached worktree failed before
  compilation because the worker sync omitted the untracked vendored Redis command
  metadata tree required by `fr-command/build.rs`
  (`legacy_redis_code/redis/src/commands`). The failed log is kept at
  `artifacts/optimization/frankenredis-bold-verify-coda/20260620T2032Z-frpersist-zset-dump-baseline/build-release.log`.
- Local fallback used a symlink to the shared vendored Redis oracle and an
  isolated target under the requested root,
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a/local-f20a92ec0`.
  The requested exact target root was not cleaned because it contained artifacts
  built by a different nightly and deleting them would violate checkout rules.

Baseline Redis 7.2.4 head-to-head:

| artifact | gate | fr/redis ratio | verdict |
|---|---|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T2032Z-frpersist-zset-dump-baseline/` | `fr-bench --workload dump`, c50 p128 n300k trials=7, 10k compact zsets x 64 members | 0.588915x | LOSS |
| same | zset-only `collection_reload_headtohead.py`, `DEBUG RELOAD` save+load | 0.308x | LOSS |
| same | zset-only DUMP encode half | 0.801x | LOSS |
| same | zset-only RESTORE decode half | 0.212x | LOSS |

Candidate idea: exploit the runtime/RDB invariant that `store_to_rdb_entries`
hands sorted-set members to `fr-persist` in score/member order. The old
`encode_compact_zset_listpack` always allocated `Vec<(&[u8], f64)>` and sorted it
again. The kept hunk detects already-sorted input and streams directly from the
owned member vector, while preserving the old canonical sort path for arbitrary
callers. This is the structural/sorted-input path, not a retry of the previously
rejected score integer-entry shortcut.

Measured keep evidence:

| artifact | gate | result | verdict |
|---|---|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T2048Z-frpersist-zset-presorted-fastpath/control-rdb-codec-bench.log` | control `cargo bench -p fr-persist --bench rdb_codec -- encode_rdb` | 4.2904 ms | baseline |
| `.../candidate-rdb-codec-bench.log` | candidate same bench/options | 3.9765 ms | 1.0789x candidate/control WIN |
| `.../zset-reload-headtohead.log` | candidate zset-only `DEBUG RELOAD` vs Redis | 0.451x | still LOSS vs Redis; ratio is noisy because Redis median shifted |
| same | candidate zset-only DUMP encode half | 0.770x | LOSS; DUMP is mostly `fr-store::dump_key`, not this fr-persist hunk |
| same | candidate zset-only RESTORE decode half | 0.217x | LOSS; decode remains the larger reload drag |

Correctness/quality:

- `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo test -p fr-persist encode_rdb_compact_zset -- --nocapture` passed; new byte-equality guard:
  `encode_rdb_compact_zset_presorted_input_is_byte_identical`.
- `cargo fmt -p fr-persist --check` passed.
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo check -p fr-persist --all-targets` passed.
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo clippy -p fr-persist --all-targets -- -D warnings` passed.
- Local fallback `cargo test -p fr-conformance -- --nocapture` passed with the
  vendored Redis symlink; existing tolerant live-oracle drift remained non-fatal.

Decision: keep the fr-persist presorted zset RDB fast path because the
server-free per-crate encoder A/B is a clear win (`1.0789x`). Do not count this
as DUMP parity or reload domination: Redis still wins the end-to-end zset DUMP
and reload gates. Next routes are `fr-store::dump_key` structural retained/cached
compact-zset payloads and RESTORE/decode listpack rebuild costs.

## 2026-06-20 cod-a kept ZADD plain-owned store fast path; runtime-only shortcut rejected

Harness: vendored Redis 7.2.4 `redis-benchmark`, same-host fresh processes,
P16, c50, n150k, interleaved trials, `connected_slaves=0`. Release binaries
were built through RCH under
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`.

Fresh Redis-relative refresh before this lever confirmed the active losses:

| artifact | command | fr/redis | verdict |
|---|---|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T2102Z-current-list-set-zset-refresh/current_vs_redis_p16_c50_n150k_trials7.txt` | lpush | 0.80x | LOSS |
| same | rpush | 0.85x | LOSS |
| same | sadd | 0.87x | LOSS |
| same | zadd | 0.73x | LOSS |
| same | set | 1.01x | parity |
| same | get | 1.04x | win |
| same | hset | 1.03x | win |
| same | incr | 1.03x | win |

Rejected attempt: changing the runtime plain-ZADD borrowed path to call the
generic default store option engine more directly. Same-window A/B showed a
target regression, so the hunk was reverted.

| artifact | command | candidate/control | candidate/redis | control/redis | verdict |
|---|---|---:|---:|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T2106Z-zadd-plain-store-candidate/candidate_control_redis_p16_c50_n150k_trials9.txt` | zadd | 0.9662x | 0.6927x | 0.7231x | rejected loss |

Kept lever: add `Store::zadd_plain_owned` for flagless `ZADD key score member
...` after the runtime parser already owns member buffers. The store fast path
skips the option engine, builds a single-member zset without an insert/search
round trip, de-duplicates missing-key multi-member input without extra member
clones, and uses insert-result enums so unchanged scores avoid write touches.

| artifact | command | candidate/control | candidate/redis | control/redis | verdict |
|---|---|---:|---:|---:|---|
| `artifacts/optimization/frankenredis-bold-verify-coda/20260620T2139Z-zadd-plain-owned-store-final/candidate_control_redis_p16_c50_n150k_trials9.txt` | zadd | 1.1075x | 0.8021x | 0.7537x | kept win |
| same | sadd | 1.0179x | 0.9268x | 0.8642x | neutral/win guard |
| same | lpush | 0.9827x | 0.7944x | 0.8218x | neutral guard; still Redis loss |
| same | rpush | 1.0178x | 0.8636x | 0.8471x | neutral/win guard; still Redis loss |
| same | set | 1.0207x | 1.0138x | 1.0438x | neutral/win guard |
| same | get | 1.0000x | 0.9786x | 0.9613x | neutral guard |
| same | hset | 0.9932x | 1.0068x | 0.9934x | neutral guard |
| same | incr | 1.0496x | 1.0208x | 1.0680x | neutral/win guard |

Correctness/quality:

- Focused store equivalence test passed:
  `cargo test -p fr-store zadd_plain_owned_matches_default_option_engine -- --nocapture`.
- `cargo check -p fr-store -p fr-runtime --all-targets` passed via RCH.
- `cargo fmt -p fr-store -p fr-runtime --check` and `git diff --check` passed.
- `cargo clippy -p fr-store -p fr-runtime -p fr-server --all-targets -- -D warnings` passed via RCH.
- `cargo test -p fr-conformance -- --nocapture` passed via RCH; `core_zset`
  live oracle reported `324/324`.

Decision: keep the store-level fast path. This is a real measured target win,
but not release domination: ZADD remains below Redis 7.2.4 (`0.8021x`). Next
routes should attack deeper sorted-set storage/index costs and the independent
list/set write losses rather than retrying runtime-only ZADD dispatch shortcuts.
## 2026-06-20 CobaltCove (cc) — `modification_count` sidecar (shrink hot `Entry`) — MEASURED LOSS, reverted

Lever: move the per-`Entry` `modification_count: u64` (WATCH/HLL-cache/mem-estimate
epoch) out of the hot keyspace `Entry` (48→40B) into a sparse
`key_modification_counts: HashMap<StoreKey,u64>` sidecar (row allocated lazily on
first overwrite/mutation/removal; fresh SET keys pay 0). Targets the keyspace RSS
gap. WATCH correctness verified sound (sidecar count strictly monotonic per key
identity, never under-aborts; HLL/mem caches `.remove(key)` on delete). Compiled
clean. A/B fr-OLD = HEAD `a8b6c3a63` vs fr-NEW sidecar (single-thread, mimalloc):

| gate | result | verdict |
|---|---|---|
| `used_memory` (reported INFO/scorecard metric) | UNCHANGED (modeled estimate, blind to struct size) | no win on the reported metric |
| RSS write-once (1M×64B) | NEW ~16–20MB / ~7% lower (noisy) | marginal RSS win, write-once only |
| RSS full-overwrite churn | NEW ~+50MB (1M sidecar rows mimalloc won't free) | regression |
| overwrite-SET throughput (best-of-6 ×3, 1.6M SETs) | OLD 720–759k vs NEW 477–634k sets/s (NEW best < OLD worst, −16..−25%) | **regression** |

Decision: reverted. Trading a noisy write-once-RSS win that doesn't move the
reported `used_memory` for a −16..−25% SET-overwrite throughput regression + churn
RSS regression is a net loss. A real Entry-RAM win needs WATCH to stop using a
per-key counter (Redis dirties watching clients directly — fr-runtime redesign).
Recorded long-form at `docs/perf_negative_evidence_ledger.md` (commit `ce56e51d7`).

## 2026-06-20 CobaltCove (cc) — SINTER/SINTERSTORE redis-style fresh-build (3+ sets) — MEASURED WIN, shipped `417c0193f`

Lever: `sinter_value` cloned the whole smallest set then `retain`-removed rejects
against each other set. Redis's `sinterGenericCommand` walks the smallest set once
and emits only survivors. Fresh-build (gated to `keys.len() >= 3`, i.e. ≥2 other
sets) avoids the intermediate result sets + extra per-other-set retain passes.
2-set and intset-smallest paths keep clone + (galloping) retain. perf blocked at
`kernel.perf_event_paranoid = 4`; used best-of-5 same-run timing.

| command | A/B | result | verdict |
|---|---|---:|---|
| SINTER over 3 string sets (2000-elem) | fr-NEW3 vs fr-OLD, best-of-5 ×3 | 4520→5760 ops/s (**+25%**, reproducible) | **keep** |
| SINTERSTORE 2 sets (2000-elem) | fr-NEW3 vs fr-OLD, best-of-5 ×3 | ~4460→~4500 ops/s (parity) | no regression (gated out) |
| SINTERSTORE 2 sets vs Redis 7.2.4 | OLD ~222µs vs Redis ~210µs (~0.95x) | the broad-sweep "0.56x" was sweep NOISE; 2-set is ~parity | do not chase 2-set |

Byte-exact: fr-OLD vs fr-NEW3 differential 0 diffs / 2000 ops (1–4 sets,
int/string/missing/wrongtype); LFU-bump tests pass; `fr-conformance` core_set +
core_set_live_redis green (99 passed). Complements BlackThrush's store-wrapper
`a3310a98d` (which optimized only the destination build, not the intersection).

## 2026-06-20 CobaltCove (cc) — wide head-to-head (GEO / collection-read / string) — NO clean lever, surface saturated

Probed less-covered families to find a fresh algorithmic gap (fr HEAD `502264773`
vs Redis 7.2.4, pipelined ×100, best-of-9). All compute-heavy paths are at parity;
the only sub-parity cells are sub-5µs dispatch-bound micro-commands (constant
per-command machinery in fr-runtime dispatch, not removable algorithmic waste —
the `ohsk5` domain), so none clear the Score≥2.0 bar.

| command | ratio fr/redis | note |
|---|---:|---|
| GEOSEARCH BYRADIUS / BYBOX (500-member) | 1.01 / 1.00 | parity — do not chase |
| GEOPOS / GEOHASH | 1.07 / 0.88 | geopos faster; geohash sub-µs dispatch |
| GEODIST | 0.60 | sub-5µs; `{:.4}` dragon-format ~28% already DECLINED on round-half-to-even byte-exactness risk (ledger) + dispatch |
| HGETALL / HKEYS / SMEMBERS (1–2k) | 1.01 / 0.99 / 0.99 | parity — collection reads not a gap |
| HRANDFIELD n=50 | 1.11 | fr faster |
| ZRANGEBYLEX / ZRANGE BYSCORE+LIMIT | 1.00 / 1.02 | parity |
| OBJECT ENCODING / GETRANGE-mid / SETRANGE | 0.81 / 0.79 / 0.84 | all sub-2µs dispatch-bound |
| BITCOUNT range | 1.14 | fr faster |

Conclusion: the clean (non-contended, non-structural) algorithmic perf surface is
exhausted. fr is parity-or-faster on every compute-heavy command across set/zset
algebra, GEO queries, collection reads, and string ops. Remaining sub-parity cells
are (a) dispatch-bound micro-costs in fr-runtime (`ohsk5`, BlackThrush), (b)
structural RAM/RDB levers (`uhthd` keyspace + PackedZSet = cod-b; ChunkedList list
DUMP; fr-persist direct-emit = cod-a), or (c) already-declined (geodist format,
zcount). No further clean cc lever this pass.

### Hash-value RAM is keyspace-dominated, NOT a PackedStrMap lever (cc follow-up)
Investigated the per-type RAM losses. Clean pipe-load (NOT Lua eval — a 600k-HSET
single `eval` blew mimalloc to a false 15x; pipe-load is the truth) of 2000 hashes
× 300 listpack fields: fr RSS +29MB vs redis +13MB (~2.2x). But `PackedStrMap` is
already a pure flat `Vec<u8>` arena (varint-len field+value inline, no per-entry
index) — i.e. structurally equivalent to a redis listpack. The 2.2x is **keyspace
overhead**: ~2000 keys × fr's heavy per-key cost (ordered_keys + dict + Arc
side-indices, the `uhthd` 4.49–5.4x gap) ≈ 14MB, plus `Vec` doubling slack on the
buffers (~1.3x). Listpack hashes cap at `hash-max-listpack-entries` (≤512), so a
hash can't be made large enough for its buffer to dominate the keyspace term —
**fr's listpack-hash RAM gap is inherently the keyspace gap (`uhthd`, cod-b), not a
separable hash-storage lever.** The only cc-separable micro-improvement would be
`shrink_to_fit` on settled hash buffers (saves the ~1.3x Vec slack on the
buffer-only portion), but that's a small net-RSS fraction and a build-speed/RAM
tradeoff on a mutable structure. Do not chase PackedStrMap for hash RAM.

## 2026-06-20 CobaltCove (cc) — canonical redis-benchmark P16 hot-command suite (ohsk5) — measured landscape

The compute-heavy sweeps above are single-conn pipe=200; this is the canonical
`ohsk5` metric: `redis-benchmark -P 16 -n 1M -r 100k`, server taskset-pinned to
core 2, benchmark to cores 4-11, fr HEAD vs Redis 7.2.4, best-of-3/4 rps.

| cmd | fr/redis | verdict |
|---|---:|---|
| SET | **1.11** | fr faster |
| INCR | **1.07** | fr faster (a noisy single run showed 0.81 — re-run best-of-4 = 1.07; do not trust single P16 runs) |
| GET | 1.04 | parity+ |
| HSET | 1.04 | parity+ |
| LPOP | 0.95 | ~parity |
| SPOP | 0.97 | ~parity |
| ZADD | 0.97 | ~parity |
| **SADD** | **0.79** | LOSS — but the store path is already alloc-free (`insert_borrowed`/saddfast: parse+binary_search+insert, no Vec on intset/dup); residual is per-command DISPATCH (fr-runtime, `ohsk5`/BlackThrush), not a clean store lever |
| **LPUSH / RPUSH** | **0.75 / 0.72** | LOSS — ChunkedList Owned-chunk append (structural, `99fwc` packed-node rewrite = cod-a/CoralOx domain) |

Conclusion: the "~2x pipelined gap" (`ohsk5`) is CLOSED for read + most write paths
(SET/GET/INCR/HSET parity-or-faster). The residual write losses are LPUSH/RPUSH
(ChunkedList structural, cod-a) and SADD (dispatch residual on an already-optimized
store path, fr-runtime/BlackThrush). No clean uncontended cc store lever remains;
the SADD store insert is byte-for-byte already what redis does (sorted intset).
Methodology note: P16 single runs are noisy under multi-agent host load — use
best-of-N and re-confirm before recording a loss (INCR 0.81→1.07).
Addendum: **MSET (10 keys/cmd, P16) fr 236k vs redis 175k = 1.35x fr faster** —
multi-key writes are fr-dominant, no lever. Completes the P16 hot-command set.

Addendum 2 — **SADD arity sweep PROVES the 0.79x is per-command DISPATCH, not store**
(200k SADD cmds, 100k keyspace, best-of-4 wall time, fr HEAD vs redis):
| members/cmd | fr | redis | fr/redis throughput |
|---:|---:|---:|---:|
| 1 | 0.326s | 0.237s | **0.73x (loss)** |
| 8 | 0.659s | 0.762s | **1.16x (fr faster)** |
| 16 | 1.100s | 1.356s | **1.23x (fr faster)** |
The gap exists ONLY at arity 1 and INVERTS to fr-faster by arity 8 — definitive
proof that fr's per-member set-insert work is faster than redis, and the
single-member 0.79x is entirely fr-runtime **per-command dispatch fixed-cost**
(amortized away by batching). Not a store lever (saddfast is already optimal); it's
`ohsk5` dispatch territory (BlackThrush). redis-benchmark's default 1-member SADD
is the worst case for any per-command fixed-cost difference.

## 2026-06-20 CobaltCove (cc) — bitmap + HyperLogLog families — fr dominates heavy ops, no new lever

Probed the previously-unbenched bitmap/HLL families (pipelined ×50, best-of-9,
fr HEAD vs Redis 7.2.4):

| cmd | fr/redis | note |
|---|---:|---|
| BITOP AND/OR/XOR (3-4KB) | 1.54 / 2.10 / 2.18 | **fr much faster** (SWAR) |
| BITCOUNT full | 1.54 | fr faster |
| PFCOUNT 2-key (merge+estimate) | 2.86 | **fr much faster** |
| PFMERGE | 1.81 | fr faster |
| BITPOS | 0.99 | parity |
| PFCOUNT 1-key | 0.59 | sub-2µs; cache ALREADY implemented (`twdut`: `hll_cache_read` returns O(1) on valid header cache) — residual is dispatch + 3-pass header validation, not algorithmic |
| SETBIT (single bit) | 0.55 | sub-2µs dispatch micro |
| BITFIELD (incrby+get) | 0.76 | sub-2µs dispatch micro |

Conclusion: fr is parity-or-faster on every compute-heavy bitmap/HLL op (and
notably 1.5-2.9x faster on BITOP/PFCOUNT-multi/PFMERGE). The three sub-parity cells
are all sub-2µs single-element commands whose obvious algorithmic optimization is
already present (PFCOUNT cache = twdut); residual is fr-runtime dispatch
(`ohsk5`/BlackThrush). No clean uncontended cc lever in bitmap/HLL.

## 2026-06-20 CobaltCove (cc) — SINTER/SDIFF fresh-build large-hashtable correctness verification

Closed a verification gap in my shipped SINTER/SDIFF fresh-build (`417c0193f`/`502264773`):
the fresh-build path only activates for **Generic (listpack/hashtable) sets at 3+ keys**,
but my initial differential used only small (≤60-member, intset) sets. Re-verified on the
exact target path — 150 trials × {SINTER,SDIFF,SINTERSTORE,SDIFFSTORE} over 3–4 sets of
200/600/1500 string members (forcing hashtable encoding), **900 operations**:
- **fr-OLD vs fr-NEW (clone+retain vs fresh-build): 0 exact diffs** (byte-identical incl. member order)
- **fr-NEW vs Redis 7.2.4: 0 membership diffs** (SINTER/SDIFF results + stored dst SMEMBERS)

The fresh-build is now proven byte-exact across the full set-encoding spectrum (intset →
listpack → hashtable) and both result delivery (read) and stored-destination paths.

## 2026-06-20 CobaltCove (cc) — cross-verify cod-b PackedZSet compact score encoding at boundaries

Independent differential verification of cod-b's recent risky change (compact tagged
PackedZSet scores: i8/i16/i32 for exact integers + raw f64 for fractional/large/inf/nan).
Probed the exact tag-transition boundaries that could break it — ±128, ±32768, ±2^31,
2^53 float-precision (9007199254740992/...993), inf/-inf, -0, fractional, plus same-score
tie-breaks — via ZRANGE/ZRANGEBYSCORE/ZREV/ZSCORE/ZRANK/ZPOPMIN/ZPOPMAX WITHSCORES.
**60 trials × 8 ops = 480 operations, 0 diffs vs Redis 7.2.4.** cod-b's PackedZSet
score encoding is byte-exact across all encoding boundaries (score values, ordering,
tie-break, and reply formatting). Their shipped lever is sound.

## 2026-06-20 CobaltCove (cc) — cross-verify BlackThrush pubsub direct encoder (RESP2+RESP3 byte-exact)

Independent byte-level differential of BlackThrush's recent risky change (`21268d72d`
direct pubsub delivery encoder, bypassing intermediate RespFrame for message/pmessage/
smessage/invalidation). Captured raw pushed bytes from a live subscriber vs Redis 7.2.4
in both protocols:
- RESP2 (`*` array): message `*3`, pmessage `*4`, smessage `*3` — **byte-exact**, incl. binary-safe payload (`hello\x00world`)
- RESP3 (`>` push): message `>3`, pmessage `>4`, smessage `>3` — **byte-exact** (correct push-type prefix)

0 diffs across all 6 frames. BlackThrush's direct encoder is byte-exact in both
protocols. Combined with the cod-b PackedZSet score verification above and my own
SINTER/SDIFF large-set verification, **all three agents' recent risky changes are now
independently byte-verified vs Redis 7.2.4.**

## 2026-06-20 CobaltCove (cc) — cross-verify cod-a ZADD plain-store fast path (full option matrix)

Independent differential of cod-a's recent change (`0004950b7` plain ZADD store fast
path). A write fast path risks mishandling the option matrix, so probed all of
NX/XX/GT/LT/CH/INCR plus combinations (incl. invalid NX+XX, GT+LT, NX+GT) on both new
and pre-seeded members, comparing the ZADD reply AND the resulting full zset state
(ZRANGE WITHSCORES): **300 trials × 6 checks = 1800 operations, 0 diffs vs Redis 7.2.4.**
cod-a's ZADD fast path is byte-exact across the option matrix.

**Swarm verification complete:** all four recent risky changes are now independently
byte-verified vs Redis 7.2.4 — cc SINTER/SDIFF fresh-build (large hashtable sets),
cod-b PackedZSet compact scores (encoding boundaries), BlackThrush pubsub direct
encoder (RESP2+RESP3), and cod-a ZADD plain-store fast path (option matrix). 0 diffs
across all.

## 2026-06-20 CobaltCove (cc) — profiling environment is fully locked (perf + ptrace), confirmed empirically

To pin the SADD/keyed-values per-command dispatch fixed-cost (arity-sweep-proven, not
a store cost), I tried every unprivileged profiling path and all are blocked here:
- **perf**: `kernel.perf_event_paranoid = 4` → hardware counters denied unprivileged.
- **gdb attach** (`gdb -p PID`): `kernel.yama.ptrace_scope = 1` → "Could not attach to
  process" (can only trace own children).
- **gdb child** (`gdb --args fr ...`): allowed by ptrace_scope, but reliable sampling
  needs non-stop/async-mode scripting; `-ex run` blocks the batch and a clean
  poor-man's sampler didn't capture frames in the time budget.
- **valgrind/callgrind**: not installed.

Conclusion: the SADD arity-1 / LPUSH / RPUSH single-element dispatch fixed-cost
(`ohsk5`) cannot be line-pinned in this sandbox without an operator unblocking
`perf_event_paranoid<=1` or `ptrace_scope=0`, or installing valgrind. Code-reading
already showed the SET vs keyed-values borrowed paths are structurally identical and
the metrics fns equivalent on the fast path, so the residual is diffuse per-command
machinery, not a single removable line. Routed to BlackThrush (fr-runtime/`ohsk5`).

## 2026-06-20 CobaltCove (cc) — DISK-LOW pause + artifact reclaim (no code lever available)

Operator flagged DISK-LOW (~56G free, 98% full) and paused new rch/cargo build+bench.
Status this turn:
- No clean cc-ownable code lever exists to implement (exhaustively established this
  campaign: every command family measured, all losses root-caused to peer-owned/
  structural domains — SADD=dispatch fixed-cost, LPUSH/RPUSH=ChunkedList, RAM=keyspace).
- With builds paused I cannot compile-verify any change; blind-committing unverified
  code to shared `main` would risk breaking the build for all agents, so none committed.
- Reclaimed my own disk artifacts to help: removed `fr-old-wt` worktree (914M), pruned
  14 stale worktree entries, cleared redundant `/tmp` binaries. The dominant disk
  consumers are the per-agent 6G `.rch-targets/*` build dirs (peer-owned).
Holding for the unblock that produces real work: a structural-bead reassignment
(`uhthd`/`99fwc`/`ohsk5`) or profiling unblock — both proven necessary, neither
self-actionable. Resume benches when disk recovered.

## 2026-06-20 CobaltCove (cc) — DISK-LOW reclaim: freed 6.8G of own build cache

Disk hit ~98% (54-56G free). Freed 6.8G by `cargo clean` on my idle build targets
(`frankenredis-cc` 6.6G + `frankenredis-old` 173M) — safe since builds are paused, the
caches were idle, and they rebuild on recovery. Disk 56G→62G free. The dominant
remaining consumers are the other per-agent 6G `.rch-targets/*` build dirs and dozens
of stale `.worktrees/.scratch` checkouts (peer-owned). Still no clean cc code lever to
implement, and no blind code commit under the build-pause (would risk shared `main`).

## 2026-06-21 CobaltCove (cc) — DISK root-cause: crisis is OTHER projects, not frankenredis

Disk still dropping (50G, 98%). Surveyed `.rch-targets/*`: the dominant consumers are
NON-frankenredis project build targets — frankenjax-cod-a 51G + frankenjax-cod-b 48G +
frankenjax-cod-a-local 35G (~134G), frankentorch-cod-a/cc ~78G, frankenfs-cc 44G,
frankenpandas-cc 27G, frankenlibc-cod-b 27G, frankenscipy-cod-a 23G. frankenredis's
own footprint is small by comparison (frankenredis-cod-b 31G is the largest, peer-owned;
my frankenredis-cc is already cleaned/empty). All `/data/tmp` frankenredis worktrees are
peers' (coralox/cod-b). I have reclaimed everything safely mine (6.8G last turn). The
remaining headroom must come from those other-project caches (cross-project decision,
not frankenredis-cc-actionable). No frankenredis code lever exists and none committed
unverified under the build-pause.

## 2026-06-21 CobaltCove (cc) — 99fwc LPUSH/RPUSH lever: precise design analysis (cannot implement "well" under build-pause)

Read the actual ChunkedList push path (packed_set.rs:2607 push_back_with_fill /
push_back_owned / ListChunk:2240). Root cause of LPUSH/RPUSH 0.72-0.75x confirmed at
source: the **mutable** active chunk is `ListChunk::Owned { elems: Arc<Vec<Vec<u8>>> }`
— every pushed element is a separate `Vec<u8>` heap allocation. Redis appends into a
single packed listpack buffer per quicklist node (no per-element alloc). The sealed
`ListChunk::Listpack { bytes, entries }` packed variant already exists but is only used
for immutable/sealed chunks.

The `99fwc` lever = give the mutable active chunk a **packed-append representation**
(growing bytes buffer + spans index, listpack-encoded) so appends are amortized
contiguous writes instead of per-element Vec allocs. This is NOT a bounded change: it
touches the entire ChunkedList element surface — push_back_owned/push_front_owned,
accepts_append (lp_bytes accounting), seal_if_owned, make_mut, pop_front/pop_back, set,
insert, remove, locate, and both iterators — and must stay byte-exact for list order,
OBJECT ENCODING, and DUMP/DEBUG (all heavily tested). It requires iterative build+test
to land safely, which is paused (DISK-LOW). It is also in `packed_set.rs` (actively
edited by cod-b for uhthd/PackedZSet) and is cod-a's assigned `99fwc` bead. Blind-
committing a partial rewrite would risk the shared build and list-data correctness, so
none committed. Owner/next step: cod-a, with build+test, on disk recovery. The simple
VecDeque variant is already in the rejected-levers ledger (measured 0.53x SLOWER).

## 2026-06-21 CobaltCove (cc) — PENDING-BENCH queue (disk-critical, builds frozen)

New peer perf code landed on origin while builds are frozen (DISK-CRITICAL ~40G); these
need differential byte-exactness + P16 A/B vs Redis 7.2.4 the instant disk recovers:
- `fdba690e2` perf(runtime): pending SADD arity-one fast path (BlackThrush — targets the
  SADD 0.79x I root-caused via arity sweep). Verify: SADD reply/state byte-exact + P16
  sadd ratio (expect arity-1 → toward parity; confirm no regression at higher arity).
- `7b94d4efc` perf(store): reduce sdiff secondary lookup (uhthd). Verify: my SDIFF
  large-hashtable differential (0-diff) still holds + SDIFF P16/3-set A/B.
- `263e3b05a` 99fwc packed-chunk blueprint (cc, design only — implement+bench on recovery).
cc verification owner for the first two on recovery; no cargo run now (disk-critical).

## 2026-06-21 CobaltCove (cc) — code-review (by inspection, no cargo) of unbenched peer perf commits on main
Reviewed both perf commits that landed during the build-freeze (live on main, not yet
benched). Both CORRECT by source inspection:
- `fdba690e2` SADD arity-1 fast path: new `execute_plain_keyed_values_write_fast_path`
  wrapper routes `Sadd && values.len()==1` → `execute_plain_sadd_one_borrowed`, ELSE falls
  through to the generic variadic path (multi-member SADD / LPUSH / RPUSH unaffected — no
  member-drop). Fast-path body is byte-equivalent to the generic path (same gates,
  `store.sadd(key,&[member])`, stat/metrics/reply/error-stats). Plain-mode gates + fallback
  intact. ✓
- `7b94d4efc` sdiff secondary-lookup reduction (in my sdiff_value Pass A): moves the
  per-other-key `contains_key` INSIDE the `lfu_tracking_enabled` branch. Verified all cases:
  LFU-on missing→continue (rng-sequence preserved), LFU-off missing→`get_mut(None)` no-op
  (continue was redundant), existing Set→touch, existing non-Set→WRONGTYPE in order. My
  fresh-build Pass A byte-exactness + sdiffwt WRONGTYPE ordering preserved. ✓
Both safe to bench/ship on disk recovery (queued above). Inspection only; full P16 A/B +
differential still owed on recovery.

## 2026-06-21 CobaltCove (cc) — BUG FOUND via new list-ops differential harness (no-cargo, frozen turn)

Built `scripts/list_ops_differ.py` (list-command differential to verify the pending 99fwc
+ zero-decode-RESTORE levers on recovery) and ran it vs Redis 7.2.4 (existing fr binary,
no cargo). 3394 checks, **11 diffs — all one real bug:**

**`list RESTORE encoding downgrade`**: fr RESTORE of a quicklist DUMP returns
`OBJECT ENCODING = listpack` where Redis returns `quicklist`, when `list-max-listpack-size`
is small (test used 4) and the list exceeds it. Logical content is CORRECT (all LRANGE
xrestore_state checks pass — fr parses the RDB fine); the *directly-built* list encoding
matches redis (the build path respects the cap); ONLY the RESTORE path diverges — fr
re-derives list encoding apparently with the default 128 threshold instead of the
configured `list-max-listpack-size`, downgrading quicklist→listpack. Byte-observable via
OBJECT ENCODING. Class: same family as the SET RESTORE re-encode gap (bbyfz, fixed) — the
list RESTORE path likely needs to honor the configured list-max-listpack-size (or preserve
the dump's quicklist encoding) like the build path does.

PENDING (disk-frozen, no cargo): locate the list RESTORE encoding-derivation
(fr-persist/fr-store list load path) and make it respect list-max-listpack-size, then
verify with this harness (0 diffs) + fr-conformance core_list. The harness is committed but
NOT yet registered in parity_suite (it currently surfaces this open bug); register after fix.
Verify on recovery whether the divergence also occurs at the default cap=128 (large lists).

### list RESTORE encoding bug — fix localization (cc, for one-shot landing on recovery)
Narrowed the RESTORE quicklist→listpack downgrade (found above) to the encoding decision
for bulk-built/restored lists under a NON-default `list-max-listpack-size`:
- `Store::object_encoding` (lib.rs:7992-8020): for non-`-2` fill it trusts
  `encoding_decided_by_write()`→`is_forced_quicklist()` first, else falls to
  `list_fits_legacy_listpack_size()` (which DOES use the configured fill correctly via
  `quicklist_packed_node_fits`). So the divergence means a restored list either (a) has
  `decided_by_write=true` with `forced_quicklist` computed under the wrong budget, or (b)
  `quicklist_packed_node_fits` mishandles a positive (entry-count) fill.
- Prime suspect: `ListValue::rebuild_growth_state` (packed_set.rs:3211-3217) sets
  `forced_quicklist = LIST_LP_OVERHEAD + raw_total > LIST_DEFAULT_BUDGET` — the **8KB
  DEFAULT**, ignoring the configured `list-max-listpack-size`. If RESTORE
  (`from_restored_quicklist2_nodes`) also marks `decided_by_write`, object_encoding trusts
  this default-budget flag and reports listpack for a small-but-over-the-configured-cap list.
- Fix candidates (verify w/ build+test + scripts/list_ops_differ.py on recovery): make the
  bulk/RESTORE path NOT set `decided_by_write` (so object_encoding falls through to the
  fill-correct `list_fits_legacy_listpack_size`), OR thread the configured fill into
  `rebuild_growth_state`. Mirrors the SET RESTORE re-encode fix (bbyfz). Severity: narrow
  (non-default list-max-listpack-size); confirm whether default cap=128 also diverges.

### list RESTORE encoding bug — ROOT CAUSE PINNED (cc; corrects earlier candidate)
Read the full path. `quicklist_packed_node_fits` (lib.rs:22135) is CORRECT (positive fill:
`entries.len() > fill → false`), so `list_fits_legacy_listpack_size` is fine. The actual
root cause is **RESTORE not preserving redis's one-way listpack→quicklist STICKINESS**:
- Redis: build a list past `list-max-listpack-size` → quicklist; popping back below the
  threshold keeps it quicklist (sticky, never converts back). RESTORE preserves quicklist.
- fr: `ListValue::from_restored_quicklist2_nodes` (packed_set.rs) sets `decided_by_write=false`
  + `fill=-2`, then `rebuild_growth_state`. With a non-`-2` configured `list-max-listpack-size`,
  `object_encoding` (lib.rs:7998) sees `decided_by_write()==false` → falls to
  `list_fits_legacy_listpack_size`, which RE-DERIVES from CURRENT contents — so a
  crossed-then-shrunk list (e.g. 130→pop→127 @ cap=128) re-derives to listpack and
  DOWNGRADES, diverging from redis's preserved quicklist. (Empirically: harness shows
  redis=quicklist, fr=listpack; logical contents identical.)
- Fix (needs build+test on recovery, verify with scripts/list_ops_differ.py): RESTORE of a
  quicklist that the RDB indicates was quicklist-encoded should mark the restored list as
  forced/sticky-quicklist (set `decided_by_write`+`forced_quicklist` under the configured
  fill) rather than re-deriving from current contents — mirroring redis's load-time
  preservation. Care: must NOT over-convert genuinely-small single-listpack-node lists that
  redis WOULD convert to listpack on load (the lsetql/a0p5p hysteresis boundary). This is
  exactly why it needs empirical build+test, not a blind edit.

### list RESTORE encoding bug — scope CONFIRMED list-specific (cc)
Probed hash/zset/set encoding-after-shrink AND encoding-after-RESTORE under non-default
{hash,zset,set}-max-listpack-entries = 4/128, n = 6/10/200 (build past cap → shrink to 3 →
DUMP → cross-RESTORE → OBJECT ENCODING): **36 checks, 0 diffs.** So hash/zset/set correctly
preserve one-way listpack→hashtable/skiplist stickiness on RESTORE (SET via bbyfz). The
RESTORE-stickiness loss is **LIST-ONLY** — fix is isolated to the quicklist RESTORE path
(`from_restored_quicklist2_nodes` + the bulk-build encoding re-derivation), no analogous
hash/zset/set work needed. Verification harness: scripts/list_ops_differ.py (lists) +
this enc_restore probe (other types, clean).

### list encoding-on-RDB bug (10ovx) — BROADER + BIDIRECTIONAL (cc deepening)
Probed COPY + DEBUG RELOAD list encoding (build past cap → shrink → check OBJECT ENCODING),
caps 128/4/-2, n=130/10/400/200 → 60 checks, 2 diffs — both DEBUG RELOAD, OPPOSITE direction
to the RESTORE finding:
- **COPY: clean** (encoding + state match redis; the bulk-build COPY path is fine).
- **DEBUG RELOAD: redis=listpack, fr=quicklist** for a 130→127 list at **cap=128 (redis's
  actual default) AND cap=4** — redis CONVERTS the crossed-then-shrunk quicklist DOWN to
  listpack on RDB-load (it now fits), fr OVER-KEEPS quicklist.
- vs. RESTORE-of-dump (list_ops_differ): fr DOWNGRADES to listpack, redis keeps quicklist.

So fr's list encoding across bulk-build paths is INCONSISTENT with redis and bidirectional:
COPY✓ / RESTORE✗(fr downgrades) / RELOAD✗(fr over-keeps), and it bites at the **default
cap=128**, not just exotic configs. Implication for the 10ovx fix: it is NOT a simple
"preserve quicklist on load" — redis's RDB-LOAD path runs listTypeTryConversion (converts to
listpack when it fits) while its RESTORE-of-a-multi-node-dump preserves quicklist; fr must
match BOTH per-path behaviors. This is subtle and bidirectional → definitively needs
build+test (cannot be safely guessed blind). Harnesses: scripts/list_ops_differ.py (RESTORE
direction) + the COPY/RELOAD probe here. Bead frankenredis-10ovx scope now covers RESTORE,
DEBUG RELOAD, and the redis-default cap=128.

### encoding/config/RDB differential sweep — CONCLUDED (cc); only 10ovx found
Completed a focused differential sweep of the encoding × config × RDB-path space (the
under-covered area where 10ovx surfaced), all vs Redis 7.2.4 (no-cargo, existing binary):
- entry/size-cap stickiness (build past cap → shrink → live/RESTORE/RELOAD/COPY): list✗
  (=10ovx, RESTORE+RELOAD, bidirectional, default cap=128); hash/zset/set ✓ (0 diffs).
- per-VALUE caps (hash/zset/set-max-listpack-value 64/16, one oversized element →
  hashtable/skiplist, live+RESTORE+RELOAD): **36 checks, 0 diffs — clean.**
- COPY list encoding: clean.
Conclusion: fr's OBJECT ENCODING is byte-exact with redis across the config/RDB matrix
EXCEPT the single list RDB-round-trip stickiness bug (10ovx). The encoding-differential vein
is now mined out — do not re-probe; the one open item is 10ovx (needs build+test to fix,
match redis per-path RDB conversion). Harnesses committed: list_ops_differ.py + the
enc_restore / copy_reload / valcap probes (in /tmp, can be promoted to scripts/ if wanted).

### NEW finding via consolidated gate: fr DEBUG RELOAD doesn't re-derive encoding (hash/set/list)
Built scripts/encoding_rdb_differ.py (permanent encoding × config × RDB-path gate; 78 checks,
0 regressions, 8 known divergences) and it surfaced 6 cases my targeted probes missed:
- **hash + set DEBUG RELOAD**: redis=listpack, fr=hashtable for a shrunk collection (built
  past cap → shrunk below). Same direction as the list RELOAD case.
- Coherent root cause: **fr DEBUG RELOAD preserves the sticky in-memory encoding** rather than
  re-deriving like redis's RDB-load does (which converts a now-fits collection back to
  listpack). Confirmed by contrast: hash/set **RESTORE-of-dump re-derives correctly** (clean),
  only DEBUG RELOAD diverges — so fr's DEBUG RELOAD likely isn't a true encoding round-trip.
  zset RELOAD is clean. (Distinct from 10ovx, which is list RESTORE-of-dump downgrade.)
- Severity: DEBUG RELOAD is a debug/test command (lower severity than a data path); matters
  for test-parity + simulating server-restart encoding. PENDING (verify on recovery whether
  fr DEBUG RELOAD should re-derive encoding to match redis; if so, route the re-derivation
  through the same load-time conversion redis uses). Gate marks these KNOWN so it catches
  true regressions. Encoding-RDB differential space now has a committed permanent gate.

### EXPIRE option matrix — verified byte-exact (cc, no-cargo)
Probed EXPIRE/PEXPIRE/EXPIREAT/PEXPIREAT × {NX,XX,GT,LT + combos} on keys with/without
existing TTL, edge cases (negative/zero/past/large), 200 trials × 3 checks = 600 vs Redis
7.2.4: the command return values + EXISTS are **byte-exact (0 real diffs)**. The only diffs
were PTTL ±1ms (8 cases) = cross-server timing jitter (PTTL read a fraction of a ms apart),
NOT a bug — future PTTL-comparing probes should allow a few-ms tolerance or compare seconds.
EXPIRE-options parity confirmed; do not re-probe.

### warm per-crate verification (cc, directive loosened to allow warm benches)
Using my still-warm cc-localbench target (warm benches now permitted; no cold rebuild):
- **fr-store unit tests GREEN at HEAD: 654 passed / 0 failed / 3 ignored** — verifies cod-b's
  sdiff-lookup (7b94d4efc) + PackedZSet score changes are unit-clean (partial peer-commit
  verification; full P16/server differential still owed on full recovery, needs release binary).
- Refined 10ovx fix scope: `ListValue::from_restored_quicklist2_nodes` (packed_set.rs:3381) is
  the SHARED RESTORE + RDB-file-load + replica-sync list-decode path (single caller lib.rs:21214);
  redis may treat RESTORE-of-dump vs RDB-file-load differently, so the fix must be verified
  across all three with the full server harness (release binary) — warm fr-store unit tests
  alone are insufficient. Fix deferred to full disk recovery accordingly.
- DEBUG RELOAD nuance: fr DEBUG RELOAD intentionally round-trips IN-MEMORY (test
  debug_reload_no_persistence_round_trips_in_memory_per_upstream), preserving encoding; the
  earlier reload encoding-divergence is likely a save-vs-nosave mode nuance, not a clear core
  bug — DOWNGRADE its severity vs the RESTORE 10ovx (which is a real cross-engine RESTORE diff).

### 10ovx list RESTORE encoding bug — FIXED (cc, disk recovered)
Fixed in `ListValue::from_restored_quicklist2_nodes` (packed_set.rs): preserve `quicklist`
encoding for a multi-node QUICKLIST_2 RDB payload (set forced_quicklist+decided_by_write when
nodes.len() > 1) instead of re-deriving from total content. redis only emits >1 node once a
list crossed list-max-listpack-size and preserves that encoding on RESTORE/RDB-load/replica;
fr was downgrading a crossed-then-shrunk quicklist to listpack. Single-node payloads still
re-derive (listpack iff they fit the configured cap), so genuinely-small lists are unaffected.
VERIFIED: fr-store unit tests 654 passed (no hysteresis regression); scripts/list_ops_differ.py
3394 checks 0 diffs (was failing); scripts/encoding_rdb_differ.py 0 regressions; fr-conformance
core_list + core_list_live_redis green. The encoding_rdb gate's list RESTORE check is now
must-pass (catches regressions). RESIDUAL (murky, downgraded severity): DEBUG RELOAD encoding
— fr round-trips in-memory (preserves) vs redis save+load re-derives; likely a save-vs-nosave
mode nuance, left as KNOWN in the gate, NOT addressed by this fix.
