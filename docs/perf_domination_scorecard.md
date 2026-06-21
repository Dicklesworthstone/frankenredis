# FrankenRedis Perf-Domination Scorecard (vs redis 7.2.4)

## Focused cod-b set-algebra STORE overwrite keep (`frankenredis-uhthd`, 2026-06-21)

- Build: `AGENT_NAME=BlackThrush RCH_WORKER=ovh-a
  CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec --
  cargo build --release -p fr-server -p fr-bench`, remote `ovh-a`.
- Focused gate: `set_algebra_vs_redis` Criterion bench, 16-command packets,
  small intset source plus large generic source where applicable, Redis 7.2.4
  oracle from `legacy_redis_code/redis/src/redis-server`.
- Retained lever: non-empty `SINTERSTORE` / `SUNIONSTORE` / `SDIFFSTORE`
  destinations overwrite the value in place through `internal_entries_insert`
  instead of remove+insert. Empty results still remove the destination.
- Correctness guard: `set_algebra_store_nonempty_overwrite_is_not_structural`
  proves non-empty STORE overwrite does not advance keyspace generation, while
  empty-result STORE still deletes structurally.

| Criterion gate | Redis mean | FrankenRedis mean | fr/Redis time | fr/Redis throughput | verdict |
|---|---:|---:|---:|---:|---|
| `SINTERSTORE` | `728.48 us` | `284.37 us` | `0.390x` | `2.562x` | win |
| `SDIFFSTORE` | `629.46 us` | `298.02 us` | `0.473x` | `2.112x` | win |
| `SUNIONSTORE` | `6.6817 ms` | `5.8679 ms` | `0.878x` | `1.139x` | win |

Set-algebra score: **3 wins / 0 losses / 0 neutral** vs Redis 7.2.4. This
supersedes the previous cod-b set-algebra score of **2 wins / 1 loss / 0
neutral** by turning SUNIONSTORE from `0.764x` throughput into `1.139x`.

Gates: `cargo fmt -p fr-store -- --check`; RCH focused fr-store test; RCH
`cargo check -p fr-store --all-targets`; RCH `cargo clippy -p fr-store
--all-targets -- -D warnings`; RCH `cargo test -p fr-conformance --
--nocapture` (194 lib tests, all conformance bins, 99 smoke tests, doctests
passed).

## Focused set intset width-carry closeout (`frankenredis-set-intset-canonical-noalloc-acetq`, 2026-06-21)

- Build: `AGENT_NAME=BlackThrush RCH_REQUIRE_REMOTE=1
  CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a rch exec --
  cargo build --release -p fr-server`, remote `hz2`.
- Focused A/B harness: `rdb_codec_set_intset/encode_set_intset_rdb`, 900 set
  keys x 96 integer members, same-worker `ovh-a`.
- Retained lever: carry the selected intset width while parsing canonical
  integer members, then pass it to `encode_intset_blob` instead of scanning the
  parsed values twice more.
- Existing guard: compact set intset selection still matches the old
  parse+`to_string` round-trip oracle for canonical, noncanonical, overflow,
  whitespace, and invalid-UTF8 members.

| focused gate | current width carry | temporary control | ratio | verdict |
|---|---:|---:|---:|---|
| set-intset RDB encode | `788.99 us` / `1.1407 Melem/s` | old width-rescan control `910.44 us` / `988.54 Kelem/s` | `1.1540x` | keep |

Redis 7.2.4 intset-only split check (`collection_reload_headtohead.py`, 2,000
sets x 40 integer members, `--set-kind int`, 7 trials):

| Redis-relative gate | fr median | Redis median | fr/Redis throughput ratio | verdict |
|---|---:|---:|---:|---|
| `DEBUG RELOAD` save+load | `8.8 ms` | `4.1 ms` | `0.559x` | loss |
| pipelined `DUMP` encode half | `11.9 ms` | `10.9 ms` | `0.917x` | loss |
| pipelined `RESTORE` decode half | `10.8 ms` | `4.6 ms` | `0.429x` | loss |

Scorecard impact: focused width-carry A/B **1 win / 0 losses / 0 neutral**;
Redis-relative split gate **0 wins / 3 losses / 0 neutral**. Combined honest
score: **1 win / 3 losses / 0 neutral**. Keep the focused encoder win, but
route remaining set-intset persistence losses to retained intset/load
representation or RESTORE decode/rebuild rather than more decimal/width-scan
micro-cleanup.

## Focused cod-b BOLD-VERIFY rebaseline (`frankenredis-uhthd`, 2026-06-21)

- Build: `AGENT_NAME=BlackThrush RCH_REQUIRE_REMOTE=1
  CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec --
  cargo build --release -p fr-server -p fr-bench`, remote `vmi1149989`.
- Binary: `/data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis`
  sha256 `55da5f2e9d91b803531663e19bea17fcd71ddea9e676f21baa3913470fc25479`.
- Source decision: **no source hunk shipped**. The failed/rejected micro-lever
  family is now exact packed-buffer reserves, Entry-tail packing, tagged zset
  score bytes, no-expiry EXISTS branch gating, and RANDOMKEY cache-capacity
  tricks. Remaining `uhthd` work needs a whole representation/table lever.

Quick memory baseline vs Redis 7.2.4 (`scripts/memory_baseline_capture.py
--quick`, scale 20k, ports from `FR_BENCH_PORT_BASE=48551`) captured
`.bench-history/memory_baseline.latest.json` and failed its ratchet because
`string_1k` moved from stored RSS ratio `0.955x` to `1.158x`.

| data type | fr/Redis RSS | fr/Redis used_memory | verdict |
|---|---:|---:|---|
| keyspace | `1.445x` | `0.492x` | loss |
| string_1k | `1.158x` | `0.767x` | loss |
| list | `0.972x` | `0.062x` | RSS win |
| hash | `1.074x` | `0.199x` | loss |
| set | `0.994x` | `0.116x` | RSS win |
| zset | `1.130x` | `0.147x` | loss |
| stream | `1.052x` | `1.085x` | loss |

Memory score: **2 wins / 5 losses / 0 neutral** on RSS. This is smaller scale
than the 200k broad scorecard and should be treated as quick routing evidence,
not a dominance claim.

Focused set-algebra Redis 7.2.4 gate:
`AGENT_NAME=BlackThrush RCH_WORKER=vmi1149989 RCH_REQUIRE_REMOTE=1
CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec --
cargo bench --profile release -p fr-bench --bench set_algebra_vs_redis --
--noplot`.

| Criterion gate | Redis mean | FrankenRedis mean | fr/Redis time | fr/Redis throughput | verdict |
|---|---:|---:|---:|---:|---|
| `SINTERSTORE` | `766.51 us` | `361.09 us` | `0.471x` | `2.123x` | win |
| `SDIFFSTORE` | `877.24 us` | `424.35 us` | `0.484x` | `2.067x` | win |
| `SUNIONSTORE` | `9.2308 ms` | `12.078 ms` | `1.308x` | `0.764x` | loss |

Set-algebra score: **2 wins / 1 loss / 0 neutral**. SINTER/SDIFF already
dominate Redis on this focused gate; SUNIONSTORE is still the measurable set
algebra gap.

## Focused hash listpack direct-emitter closeout (`frankenredis-hash-listpack-direct-emit-dv9n5`, 2026-06-21)

- Build: `RCH_REQUIRE_REMOTE=1 CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a rch exec -- cargo build --release -p fr-server`, remote `vmi1149989`.
- Focused A/B harness: `rdb_codec_hash_listpack/encode_hash_listpack_rdb`, 600
  hash keys x 96 fields, same-worker `vmi1227854`.
- Retained lever: stream field/value entries directly into the hash listpack
  payload instead of allocating a flat `Vec<&[u8]>` staging array.
- Rejected experiment: header-in-place final-buffer emission. It was
  `1.0554x` slower than retained direct emit and was reverted.

| focused gate | current direct emit | temporary control | ratio | verdict |
|---|---:|---:|---:|---|
| hash-listpack RDB encode | `2.6388 ms` / `227.38 Kelem/s` | buffered flat control `3.0709 ms` / `195.38 Kelem/s` | `1.1637x` | keep |
| hash-listpack RDB encode | `2.6388 ms` / `227.38 Kelem/s` | final-buffer variant `2.7849 ms` / `215.44 Kelem/s` | `0.9475x` | reject |

Redis 7.2.4 hash-only split check (`collection_reload_headtohead.py`, 2,000
hashes x 40 fields, 7 trials):

| Redis-relative gate | fr median | Redis median | fr/Redis throughput ratio | verdict |
|---|---:|---:|---:|---|
| `DEBUG RELOAD` save+load | `19.4 ms` | `6.7 ms` | `0.344x` | loss |
| pipelined `DUMP` encode half | `14.7 ms` | `10.6 ms` | `0.720x` | loss |
| pipelined `RESTORE` decode half | `14.2 ms` | `6.7 ms` | `0.473x` | loss |

Scorecard impact: focused direct-emitter A/B **1 win / 0 losses / 0 neutral**;
rejected final-buffer experiment **0 wins / 1 loss / 0 neutral**;
Redis-relative split gate **0 wins / 3 losses / 0 neutral**. Combined honest
score: **1 win / 4 losses / 0 neutral**. Keep the direct emitter, but route
remaining hash persistence losses to retained/listpack representation and
RESTORE decode/rebuild rather than another vector-elision micro-pass.

## Focused cod-b packed bulk exact-capacity rejection (`frankenredis-uhthd`, 2026-06-21)

- Build: fail-closed remote `rch` release builds for `fr-server` and `fr-bench`
  with `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`.
- Harness: fresh-process hash/zset memory probe against vendored Redis 7.2.4,
  scale 200k, using the warm `frankenredis` release binary.
- Candidate: exact varint-aware packed-builder reserve sizes for
  `HashFieldMap::from_unique_pairs{,_borrowed}` and
  `PackedZSet::from_unique_pairs`.
- Decision: **rejected and source reverted**. Redis-relative ratios moved in the
  right direction only because the Redis oracle RSS was higher in the candidate
  window; FrankenRedis absolute RSS worsened on both target cells.

| memory gate | control fr/Redis RSS | candidate fr/Redis RSS | FrankenRedis absolute delta | verdict |
|---|---:|---:|---:|---|
| packed hash | `1.300x` | `1.202x` | `+557,056 B` | loss |
| packed zset | `1.555x` | `1.491x` | `+16,384 B` | loss |

Scorecard impact: **0 wins / 2 losses / 0 neutral** on the target absolute-RSS
signal. Do not retry fixed-capacity/exact-reserve tweaks for packed hash/zset
unless a same-window A/B shows real FrankenRedis RSS reduction or an allocator
class proof predicts process-RSS movement. Route the remaining hash/zset memory
gap to deeper representation/table overhead.

Infra-only note: `.rchignore` now excludes `legacy_redis_code/`, `artifacts/`,
and `.bench-history/`; after the first fail-closed RCH sync timeout, remote sync
fell to about 7.3 MB and the per-crate release build completed. That is build
hygiene, not a Redis performance keep.

## Focused cod-b current-control memory scorecard (`frankenredis-uhthd`, 2026-06-20)

- Build: fail-closed remote `rch` build on `vmi1152480`:
  `RCH_REQUIRE_REMOTE=1 CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec -- cargo build --release -p fr-server -p fr-bench`.
- Harness: `scripts/memory_baseline_capture.py` against vendored Redis 7.2.4,
  fresh process per data type, `FR_BENCH_PORT_BASE=45251`, scale 200k.
- Raw JSON was generated at `.bench-history/memory_baseline.latest.json` in the
  worktree but not committed because that path was actively reserved by
  CobaltCove; the measured ratios are recorded below and in the Beads comment.
- Source decision: no hunk shipped. `fr-store` keyspace/packed-set source was
  under active CobaltCove reservations, so this pass records current-control
  evidence and routes the next source swing.

| data type | fr/redis RSS | fr/redis used_memory | verdict |
|---|---:|---:|---|
| zset | 1.728 | 0.619 | largest RSS loss |
| hash | 1.562 | 0.838 | loss |
| keyspace | 1.403 | 0.805 | `uhthd` loss remains |
| set | 1.303 | 0.562 | loss |
| list | 1.078 | 0.391 | small loss |
| stream | 0.978 | 1.096 | RSS win |
| string_1k | 0.903 | 0.964 | win |

Scorecard impact: current memory score is **2 wins / 5 losses / 0 neutral** on
RSS. The highest-value remaining work is not another Entry-tail or key-inline
micro-lever; it is zset/hash/keyspace representation work with same-current
A/B proof once the store files are free.

## Focused cod-b compact tagged PackedZSet score storage (`frankenredis-uhthd`, 2026-06-20)

- Build: `rch exec -- cargo build --release -p fr-server -p fr-bench`, with
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`.
- Harness: memory baseline capture against vendored Redis 7.2.4, with a ZADD
  throughput guard for the target command surface.
- Measured candidate: exact integer packed-zset scores use a tagged
  `i8`/`i16`/`i32` payload instead of always storing raw `f64` bytes.
  Fractional, large, infinite, and NaN scores remain raw `f64`.
- Artifact directory:
  `artifacts/optimization/frankenredis-uhthd-packed-zset-score-codb/20260620T1915Z/`.

| memory gate | hash | keyspace | list | set | stream | string_1k | zset | verdict |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| current-control / Redis | 1.422 | 1.405 | 1.396 | 1.093 | 0.978 | 0.931 | 1.619 | zset loss confirmed |
| rebuilt candidate / Redis | 1.205 | 1.365 | 1.195 | 1.259 | 0.980 | 0.891 | 1.456 | zset improved, still loss |
| best candidate / Redis | 1.249 | 1.489 | 1.127 | 1.141 | 0.968 | 0.924 | 1.271 | supporting target win |

ZADD throughput guard vs Redis 7.2.4: median `0.93x` (`0.93 / 1.01 / 0.59`)
under high load, so no clear throughput regression claim. A failed-ratchet memory
rerun is retained as negative evidence because list/hash/set moved worse by more
than 15% while zset stayed improved; the only keep claim is the target zset RSS
movement.

Scorecard impact: zset memory moved from `1.619x` to `1.456x` Redis-relative in
the final rebuilt pass (`0.793x` candidate/control on the target cell). This is
supporting evidence for the peer-owned source hunk, not domination. The final
memory classification is still
**2 wins / 5 losses / 0 neutral** across the seven cells; remaining structural
targets are zset/keyspace/list/hash/set layout.

cod-a recheck on the same shared hunk:

- Artifact:
  `artifacts/optimization/frankenredis-bold-verify-coda/20260620T1609Z-packed-zset-coda-verify/`.
- Per-crate gates passed under
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`: release
  build for `fr-server`/`fr-bench`, `cargo check -p fr-store --all-targets`,
  `cargo test -p fr-store zset -- --nocapture`, `cargo clippy -p fr-store
  --all-targets -- -D warnings`, `cargo test -p fr-conformance --
  --nocapture` with RCH local fallback, and `cargo fmt -p fr-store --check`.
- Read-only packed-zset RSS probe, 6,250 small zsets x 32 members:
  Redis data-RSS `4.58 MB`, FrankenRedis data-RSS `8.11 MB`, ratio `1.77x`
  fr/Redis.
- Read-only ZADD throughput guard on the same cod-a binary, Redis benchmark P16,
  c50, n150k, trials5, loadavg `11.21`: median `0.77x` fr/Redis with trials
  `0.77 / 0.64 / 0.79 / 0.82 / 0.74`.
- Verdict: negative evidence for domination. Keep the compact-score hunk only as
  a narrow target-density improvement; the next measured memory lever needs to
  remove deeper zset/keyspace/member overhead, and ZADD throughput remains below
  the `0.9x` parity floor in this recheck.
- Targeted `ubs` returned nonzero on file-wide legacy/static-analysis findings
  in `packed_set.rs`, including false-positive JWT `decode` hits on existing
  `cfm_decode` helpers. Compiler/clippy/fmt/zset/conformance gates were clean.

## Focused cod-a pubsub fanout direct encoder (`frankenredis-ohsk5`, 2026-06-20)

- Build: `rch exec -- cargo build --release -p fr-server -p fr-bench`, with
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`.
- Harness: custom pubsub fanout gate against saved FrankenRedis current-control,
  direct-encoder candidate, and vendored Redis 7.2.4. Metric is delivered
  subscriber-messages per second.
- Kept change: direct pubsub message encoding into `fr-server` connection write
  buffers, avoiding intermediate `RespFrame` allocation and re-encoding in the
  delivery hot path.
- Rejected change in the same pass: pending-pubsub client collection
  `HashSet<u64>` to `Vec<u64>` measured `0.9963x` candidate/control and was
  reverted.
- Artifact directory:
  `artifacts/optimization/frankenredis-bold-verify-coda/20260620T1823Z-pubsub-pending-vec-candidate/`.

| topology | control/redis | candidate/control | candidate/redis | verdict |
|---|---:|---:|---:|---|
| 32 subscribers, 4000 messages, pipe 32, trials 7 | 0.9390 | 1.0614 | 0.9967 | primary keep; Redis gap nearly closed |
| 32 subscribers, 4000 messages, pipe 32, trials 5 | 0.9272 | 1.0150 | 0.9411 | confirm modest win; still below Redis |
| 64 subscribers, 3000 messages, pipe 32, trials 5 | 0.9539 | 1.0242 | 0.9770 | confirm modest win; gap narrowed |

Scorecard impact: pubsub fanout moved from a measured Redis-relative loss into
near-parity on the primary shape, but confirmations still sit below Redis. Count
this as a keep and a narrowed release gap, not a completed domination cell.

Crate-bench smoke: `cargo bench --release -p fr-bench` was attempted and failed
because this Cargo rejects `--release` for `cargo bench`; the valid optimized
bench-profile command `cargo bench -p fr-bench` passed via `rch` after pinning
`FR_SERVER_BIN`. That broad Criterion run is context only and did not include
the pubsub fanout workload.

## Focused cod-b non-store GET probes (`frankenredis-ohsk5`, 2026-06-20)

- Build: `rch exec -- cargo build --release -p fr-server -p fr-bench`, with
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b` for the
  current binary and isolated target dirs for clean candidate worktrees.
- Harness: vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n150k, 7-9
  interleaved trials through `scripts/bench_vs_redis.py`.
- Current-vs-Redis artifact:
  `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T061610Z-redis-benchmark-current/current_vs_redis_redis_benchmark.txt`.
- Candidate A/B artifacts:
  `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T061925Z-resp3-cache-candidate/candidate_vs_control_get_guard_20260620T0626Z.txt`
  and
  `artifacts/optimization/frankenredis-ohsk5-codb-nonstore/20260620T0630Z-get-expire-count-gate/candidate_vs_control_get_guard_20260620T0632Z.txt`.
- Keep/revert decision: **NO SOURCE KEPT**. Both non-store GET candidates were
  noise-scale and were reverted/not applied to the shared checkout.
- Coordination: store-owned `fr-store/src/lib.rs` was reserved by BlackThrush,
  who reported the separate DUMP zset-listpack re-encode gap; this pass stayed
  on non-store server/runtime probes.

Current Redis-relative focused matrix:

| command | median fr/redis | verdict |
|---|---:|---|
| set | 1.04 | fr faster |
| get | 0.83 | **loss** |
| incr | 0.99 | neutral |
| lpush | 0.84 | **loss** |
| rpush | 0.74 | **loss** |
| lpop | 1.07 | fr faster |
| rpop | 1.24 | fr faster |
| sadd | 0.73 | **loss** |
| hset | 1.08 | fr faster |
| spop | 1.03 | fr faster |
| zadd | 0.69 | **loss** |
| mset | 1.15 | fr faster |

Rejected candidates:

| candidate | target command | target ratio vs control | guard ratios vs control | verdict |
|---|---|---:|---|---|
| Batch-local RESP3 reply-mode cache in `fr-server` | get | 1.02 | set/incr/hset/mset 1.01/0.95/0.98/1.02 | rejected |
| Skip plain-GET fast active-expire call when no expiring keys in `fr-runtime` | get | 1.01 | set/incr/hset/mset 0.99/0.97/0.95/1.01 | rejected |

Scorecard impact: focused P16/c50 score is **6 wins / 5 losses / 1 neutral**
if every listed command is counted directly. The non-store GET probes did not
move the measured `GET 0.83x` Redis-relative gap enough to ship. The largest
fresh losses route to store/data-structure lanes: `ZADD 0.69x`, `SADD 0.73x`,
`RPUSH 0.74x`, `LPUSH 0.84x`; `GET 0.83x` needs a deeper profile than reply
mode caching or active-expire no-op elision.

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

## Focused zset DUMP score-entry shortcut rejection (`frankenredis-zset-listpack-score-zero-copy-z56kl`)

- Date: 2026-06-20, cod-a.
- Target: `fr-bench --workload dump`, c50, p128, keyspace 10000, compact
  integer-scored zsets.
- Build: `rch exec -- cargo build --release -p fr-server -p fr-bench`,
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`.
- Profile route: shared BlackThrush `dump@p128` profile named `Store::dump_key`
  and listpack score-entry encode/reparse under the broader zset DUMP loss.
  Local kernel `perf` was blocked by `perf_event_paranoid=4`.
- Decision: **REJECT current form / no cod-a source kept**. Correctness guard
  passed, but the stronger low-CV confirmation regressed throughput.

| gate | artifact | ratio | cv | verdict |
|---|---|---:|---|---|
| baseline vs Redis 7.2.4 | `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T061700Z-baseline/summary.txt` | 0.616569 fr/redis | redis 5.27%, fr 3.13% | gap confirmed, Redis side slightly noisy |
| dirty candidate vs saved control | `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T062635Z-dirty-candidate-ab/summary.txt` | 1.080504 candidate/control | 4.73% / 4.96% | supporting win only |
| dirty candidate vs Redis 7.2.4 | same | 0.569797 candidate/redis | redis 16.78% | noisy Redis leg, not a keep claim |
| confirmation vs saved control | `artifacts/optimization/frankenredis-z56kl-store-dump-score-entry/20260620T062741Z-candidate-control-confirm/summary.txt` | 0.955895 candidate/control | 3.71% / 2.38% | rejected |

Scorecard impact: `dump@p128` remains a major measured loss. The next viable
route is structural retained/cached compact-zset DUMP representation or avoiding
per-DUMP rebuild from the zset's dual in-memory indexes, not another isolated
score-formatting micro-shortcut.

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

## Focused SPOP parser-ordering keep (`frankenredis-15lug.1`, Redis C client)

- Artifacts:
  - Baseline: `artifacts/optimization/frankenredis-15lug-1/20260620T053608Z-baseline/bench_vs_redis_p16_c50_n150k_trials7.txt`.
  - Kept candidate: `artifacts/optimization/frankenredis-15lug-1/20260620T054808Z-early-keyed-pop-candidate/bench_vs_redis_p16_c50_n150k_trials7.txt`.
  - Confirmation: `artifacts/optimization/frankenredis-15lug-1/20260620T054843Z-early-keyed-pop-confirm/bench_vs_redis_p16_c50_n150k_trials7.txt`.
- Harness: vendored `redis-benchmark`, P16, c50, n150k, 7 interleaved trials.
- Kept change: exact no-count `SPOP key` keyed-pop parser plus early keyed-pop parser ordering in
  `crates/fr-server/src/main.rs`.
- Profile route: `/data/tmp/claude-1000/profile_hot_path_4149131.data` showed
  `process_buffered_frames` and failed exact-parser probes ahead of keyed pop as the residual
  SPOP cost.

| command | baseline fr/redis | kept candidate fr/redis | confirmation fr/redis | verdict |
|---|---:|---:|---:|---|
| spop | 0.75 | 1.03 | 1.04 | SPOP floor fixed |
| lpop | not measured | 1.02 | not measured | parity/win side effect |
| rpop | not measured | 1.00 | not measured | neutral side effect |
| lpush | 0.78 | 0.75 | 0.78 | residual loss, separate target |
| rpush | 0.91 | 0.91 | 0.89 | noisy around floor |

### Cod-b fresh-restart confirmation (`frankenredis-15lug.1`)

- Final artifacts:
  `artifacts/optimization/frankenredis-15lug-spop-frontload-pop/20260620T055254Z-final-five-command/`
  and
  `artifacts/optimization/frankenredis-15lug-spop-frontload-pop/20260620T055340Z-final-spop-focused/`.
- Rejected exact-only artifact:
  `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T054238Z-candidate-redis/candidate_vs_redis_redis_benchmark.txt`.
- Profile route:
  `artifacts/optimization/frankenredis-15lug-spop-exact-packet/20260620T054407Z-profile-current-spop/perf_report_no_children.txt`
  showed `Store::spop` at only 0.38% self; the work stayed in parser ordering.

| gate | command | median ratio | verdict |
|---|---|---:|---|
| current baseline vs Redis 7.2.4 | spop | 0.77 | confirmed loss |
| exact-packet-only candidate vs Redis 7.2.4 | spop | 0.78 | rejected |
| final vs current-control | spop | 1.25 | keep |
| final vs current-control | lpush/rpush | 1.00 / 1.04 | no regression |
| final vs Redis 7.2.4 | spop | 1.06 | parity/win |
| final SPOP-focused vs current-control, 11 trials | spop | 1.30 | confirmed keep |
| final SPOP-focused vs Redis 7.2.4, 11 trials | spop | 1.00 | confirmed parity |

SPOP is no longer a focused parity-floor loss on this gate. `LPUSH`/`RPUSH`
remain residual list-write gaps in the Redis-relative guard and should be
handled as a separate measured lane.

Scorecard impact: the focused `SPOP` parity-floor loss is cleared for the Redis C-client gate.
The remaining measured P16/c50 residual in this lane is list push, especially `LPUSH`, not SPOP.

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

## Targeted Gauntlet: cod-b ZCOUNT compact full-zset slice count

- Commit candidate: none kept. The compact full-zset `ZCOUNT` `window.len()` fast path was measured
  in a detached clean worktree and reverted.
- Build/bench: release binaries built with
  `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`; candidate release build
  succeeded via `rch exec` on `vmi1149989`. Redis oracle was vendored Redis 7.2.4.
- Proof bundle:
  `artifacts/optimization/frankenredis-codb-zcount-compact-count/20260620T133708Z/`.
- Correctness guard: `cargo test -p fr-store score_bound_count -- --nocapture` passed for the
  isolated candidate; the test run fell back locally after rch sync timeout.
- Final source conformance after revert passed via
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo test -p fr-conformance -- --nocapture`
  on `hz2`.

| gate | `ZCOUNT` ratio | verdict |
|---|---:|---|
| control vs Redis 7.2.4 broad harness | 0.63 | target loss confirmed |
| candidate vs control broad harness | 1.03 | neutral |
| candidate vs control focused 5000-pipe/21-trial | 0.982 | rejected |
| candidate vs Redis 7.2.4 broad harness | 0.65 | still a Redis-relative loss |

Lever score: **0 wins / 1 loss / 1 neutral** on candidate/control gates. Redis-relative score stays
negative for `ZCOUNT`; broad candidate-vs-Redis also still showed `getrange` and `sintercard`
below 0.9x. Do not spend another pass on this compact-slice count shortcut unless a fresh profile
shows the sentinel-filter scan itself is the bottleneck.

## Cod-a bold-verify refresh and rejected ZADD borrowed-noop lever (MEASURED 2026-06-20)

Fresh restart, agent `CobaltCove`, vendored Redis 7.2.4, `redis-benchmark`,
P16/c50/n150k, 7 interleaved trials, release build via
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a rch exec -- cargo build --release -p fr-server`.
Artifact:
`artifacts/optimization/frankenredis-bold-verify-coda/20260620T133457Z/current_vs_redis_standard_p16_c50_n150k_trials7.txt`.

| Command | median fr/redis | scorecard result |
|---|---:|---|
| `set` | 0.98x | neutral |
| `get` | 1.01x | neutral/win |
| `incr` | 0.98x | neutral |
| `lpush` | 0.79x | loss |
| `rpush` | 0.74x | loss |
| `lpop` | 1.06x | win |
| `rpop` | 1.16x | win |
| `sadd` | 0.81x | loss |
| `hset` | 1.01x | neutral/win |
| `spop` | 1.01x | neutral/win |
| `zadd` | 0.77x | loss |
| `lrange_100` | 1.00x | neutral |
| `mset` | 0.93x | neutral |

Score with the 0.9x parity floor: **5 wins / 4 losses / 4 neutral**.
Current measured loss frontier: `RPUSH`, `ZADD`, `LPUSH`, `SADD`.

Rejected lever: a borrowed `ZADD` existing-member/no-op-score shortcut avoided
member ownership for unchanged scores, but the 9-trial guard produced
`ZADD=0.74x` vs Redis (`artifacts/optimization/frankenredis-bold-verify-coda/20260620T134553Z-zadd-borrowed-candidate/candidate_vs_redis_standard_p16_c50_n150k_trials9_zadd_family.txt`),
worse than the 0.77x pre-edit refresh. The source hunk was reverted. Next ZADD
route should be storage/index complexity reduction, not parser-side member
borrowing.

## Cod-a list LP-byte reuse candidate rejection (MEASURED 2026-06-20)

Focused follow-up for `frankenredis-ohsk5`: a duplicate listpack-size accounting
shortcut was tested on the current list-write frontier. Candidate and clean
control were both built via
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a rch exec -- cargo build --release -p fr-server`
and measured against vendored Redis 7.2.4 with `redis-benchmark`, P16/c50/n150k,
9 interleaved trials.

| Command | candidate fr/redis | control fr/redis | candidate/control | scorecard result |
|---|---:|---:|---:|---|
| `lpush` | 0.92x | 0.93x | 0.99x | rejected neutral |
| `rpush` | 0.82x | 0.87x | 0.94x | rejected loss |
| `sadd` | 0.85x | 0.83x | 1.02x | guard neutral; still loss |
| `zadd` | 0.75x | 0.77x | 0.97x | guard down; still loss |
| `lpop` / `rpop` / `lrange_100` | 1.16x / 1.15x / 1.06x | 1.15x / 1.25x / 1.05x | 1.01x / 0.92x / 1.01x | guards mixed |
| `set` / `get` / `incr` / `hset` / `mset` | 1.07x / 1.00x / 1.03x / 1.13x / 1.19x | 1.09x / 1.01x / 1.03x / 1.16x / 1.18x | 0.98x / 0.99x / 1.00x / 0.97x / 1.01x | guards neutral |

Lever score: **0 wins / 1 loss / 1 neutral** on the list-write target cells.
No source hunk remains. Current clean-control frontier in this gate:
`RPUSH=0.87x`, `SADD=0.83x`, `ZADD=0.77x`; `LPUSH=0.93x` is above the 0.9x
floor in this noisy rerun.

## Cod-b SMISMEMBER direct-encoder rejection (MEASURED 2026-06-20)

Focused follow-up for the current broad set-read frontier. A direct
`SMISMEMBER` network encoder avoided per-flag `RespFrame::Integer`
materialization while keeping the same `Store::smismember` call, metrics, and
keyspace accounting. The source hunk was reverted after timing.

| Command / gate | candidate ratio vs Redis | control ratio vs Redis | candidate/control | scorecard result |
|---|---:|---:|---:|---|
| `smismember`, broad control refresh | n/a | 0.79x | n/a | baseline loss |
| `sintercard`, broad control refresh | n/a | 0.62x | n/a | baseline loss, untouched |
| `zcount`, broad control refresh | n/a | 0.61x | n/a | baseline loss, prior lever rejected |
| `smismember`, broad candidate/control | n/a | n/a | 1.03x | neutral |
| `smismember`, focused pipe=2000 trials=21 | 0.99x | 0.93x | 0.96x | rejected loss |

Lever score: **0 wins / 1 loss / 1 neutral** on the `SMISMEMBER` target cell.
No source hunk remains. Current measured set-read frontier from this pass:
`SINTERCARD=0.62x`, `SMISMEMBER=0.79x` in the broad refresh, with `ZCOUNT=0.61x`
still a known rejected constant-factor gap. Next route should be set layout,
probe specialization, or no-LIMIT intersection counting rather than direct reply
encoding.

## BlackThrush SINTERCARD resolve-other-sets-once (MEASURED WIN 2026-06-20)

Closes the `SINTERCARD` set-read loss cell flagged in the broad refresh above
(`0.62x`). `Store::sintercard` (fr-store) re-called `self.entries.get(*key)` —
a full keyspace dict lookup (key hash + bucket probe + `Box` deref) — for **every
`(member × other-key)` pair**, i.e. `M*K` keyspace lookups layered on top of the
unavoidable `M*K` set-membership probes. `sinter_value` already resolves each set
once via `retain_intersect`; SINTERCARD did not. The fix resolves
`other_sets: Vec<&SetValue>` a single time before the member loop (both the
sequential and de-clustered LIMIT paths), so the hot loop pays only the
membership probe. All keys are present at that point (`has_empty` was false) and
every mutable LFU/touch bump already happened, so the immutable borrows coexist
with `min_set`; visited order and the count are unchanged → byte-identical.

Built candidate + clean control via
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-bt rch exec -- cargo build --release -p fr-server --bin frankenredis`;
benched `scripts/broad_command_headtohead.py` P200/trials=11 against vendored
Redis 7.2.4, 3 interleaved runs each, same redis instance.

| Command / gate | candidate vs Redis | control vs Redis | scorecard result |
|---|---:|---:|---|
| `sintercard` (run 1/2/3) | 1.09x / 1.08x / 1.28x | 0.73x / 0.62x / 0.73x | **WIN — loss→domination** |
| `sintercard` fr-side ms | ~10–11 ms | ~17–18.8 ms | ~40% faster fr-side |
| `sinter3` (untouched guard) | 0.87–0.96x | 0.86–0.92x | unchanged (no collateral) |
| `smismember` (noisy guard) | 0.82–0.90x | 0.74–1.17x | unchanged/noisy |

Lever score: **1 win / 0 loss / 0 neutral**. Correctness: differential vs Redis
7.2.4 across 15 edge cases (2/3 sets, `LIMIT 0/5/20/3`, intset + listpack-small
encodings, both de-clustered and sequential paths, missing key both orders,
WRONGTYPE both orders, self-intersection) → **0 diffs**. Source hunk shipped.
Set-read frontier now: `SMISMEMBER≈0.79x` (noisy), `ZCOUNT≈0.61x` (rejected),
`SINTER` 3-way `≈0.9x` (different path, retain_intersect).

## CobaltCove SINTER/SDIFF redis-style fresh-build for 3+ sets (MEASURED WIN 2026-06-20)

Closes the `SINTER` 3-way residual flagged just above. `sinter_value`/`sdiff_value`
(fr-store) cloned the whole smallest/first set then `retain`/`retain_diff`-removed
the rejects against each other set — copying ~2x the surviving members and
materializing an intermediate result set per other-key. Redis's
`sinterGenericCommand`/`sdiffGenericCommand` instead walk the smallest/first set
once and emit only members present-in-all / absent-from-all. Gated to
`keys.len() >= 3` (≥2 other sets), where the single-pass fresh-build beats
clone+retain; the 2-set and intset-encoded paths keep clone + (galloping)
`retain_intersect`/`retain_diff` (measured parity, no regression). Touch/LFU is
done up-front in the exact prior order so the LFU rng draw sequence — and the
`sdiffwt` missing-first WRONGTYPE-checks-all-sources rule — stay byte-identical.

Built candidate vs `HEAD` control via RCH release; timed best-of-5 ×3, fr-vs-fr to
isolate the change from Redis noise.

| Command / gate | fr-NEW vs fr-OLD | scorecard result |
|---|---:|---|
| `SINTER` 3 sets (2000-elem) | 4520 → 5760 ops/s (**+25%**, reproducible ×3) | **WIN** |
| `SDIFF` 3 sets (2000-elem) | 3960 → 4675 ops/s (**+18%**, reproducible ×3) | **WIN** |
| `SINTERSTORE`/`SDIFFSTORE` 2 sets | ~parity (gated out) | no regression |
| broad `sinter3` vs Redis | 0.85x → **0.97x** | loss→parity |

Lever score: **2 wins / 0 loss / 0 neutral**. Correctness: fr-OLD-vs-fr-NEW
differential **0 diffs / 2000 ops** (1–4 sets, int/string/missing/wrongtype);
LFU-bump + `sdiffwt` tests pass; `fr-conformance` core_set + core_set_live_redis
green. Shipped `417c0193f` (SINTER) + `502264773` (SDIFF). Complements
BlackThrush's SINTERCARD/store-wrapper work (which optimized the destination build,
not the intersection algorithm). Single-element SADD/LPUSH/RPUSH P16 losses
root-caused separately to per-command dispatch fixed-cost (arity sweep: fr's
per-member store is *faster* than Redis) + ChunkedList — see NEGATIVE_EVIDENCE.md.

## BlackThrush generic-dispatch clock chaining (MEASURED, profile-driven, 2026-06-20)

Profiled fr (`perf record`, paranoid lowered then restored) under a deep-pipelined
cold-command load (SUBSTR/APPEND/GETEX/COPY/TYPE). Call-graph attribution put
**~12% of CPU in `clock_gettime`** — the largest non-syscall cost — charged to
`execute_frame_internal`. The 7grsy chained timer already collapses this to one
clock read/cmd for the ~70 borrowed fast-path handlers, but the **generic
dispatch path** (`lib.rs:16217`, the entire long tail + writes) still did
`Instant::now()` + `start.elapsed()` = **2 clock reads/cmd**.

Fix: added `chained_command_start_pre()` (adjacency `prev_seq == seq`, since the
generic path reads the clock BEFORE the command increments
`stat_total_commands_processed`, vs the fast paths' post-increment `+1`) and
routed the generic path through it + `finish_chained_command`. Reuses the prior
command's end-instant as this command's start → **1 clock read/cmd**. Also fixes
a latent chain break (generic commands previously left `last_command_end` stale,
forcing the next fast-path command to re-read). Robust to a command incrementing
the counter by !=1 (recorded post-count always equals the next pre-count).

Correctness: `cargo test -p fr-runtime --lib commandstat` (7) + `histogram` (1) +
`-p fr-server process_buffered_frames` (4, incl `uses_microsecond_clock_for_time`)
all green; live `INFO commandstats` usec stays populated and sane
(substr 5.2 µs/call, append 1.92, type 1.64).

MEASURED — `perf stat` over a FIXED 2.4M-command mixed cold load, candidate
(genclock) vs control (prior shipped), 3 interleaved rounds:

| metric | control | candidate | delta |
|---|---:|---:|---:|
| instructions retired | 21,720 M (±1 M) | 21,635 M (±1 M) | **-85 M (-0.39%), all 3 rounds** |
| cycles | ~11,229 M | ~11,018 M | **-1.9%, lower every round** |
| IPC | 1.90–1.93 | 1.94–1.99 | improved |

Server does provably less work per command (deterministic instruction-count drop,
reproducible ×3). Throughput A/B is network/client-bound at ~3 µs/call so the
wall-clock delta sits under benchmark noise (incr/type fast-path anchors stayed
neutral, confirming no regression); the win surfaces under real pipelined
saturation, which is the ohsk5 scenario. Lever score: **1 win / 0 loss / 0
neutral** (server-CPU). Helps the entire generic long tail + RPUSH/SADD/ZADD
writes (they dispatch through the same path).

## BlackThrush lazy command_name (MEASURED, profile-driven, 2026-06-20)

Re-profiled the post-genclock binary (perf record under deep-pipelined cold-command
load): `clock_gettime` had dropped from ~12% to ~2.5% (genclock fix confirmed), and
a fresh systemic cost surfaced — **`Utf8Chunks::next` at ~1.8%** of pipelined CPU,
from `String::from_utf8_lossy(&argv[0])` built EAGERLY per command in
`execute_frame_internal` as `command_name`. Every one of its 8 consumers is a cold
rejection/error branch (NOAUTH threat reason, ACL NOPERM command/key/channel,
command-time-budget warn) — none runs for a normal successful command — yet the
per-command UTF-8 lossy scan of argv[0] ran for every dispatched command.

Fix: replaced the eager `let command_name = &command_name_lossy` with a closure
`let command_name = || String::from_utf8_lossy(&argv[0])` and call `command_name()`
at the 8 cold sites. `argv` is a shared `&[Vec<u8>]` so the closure copies the
reference (no borrow conflict with the `&mut self` dispatch). Byte-identical: the
closure yields the exact same Cow; error messages unchanged.

Correctness: `acl_command` (2) + `unauthenticated` (1) + `noauth` (1) fr-runtime
tests green; builds clean.

MEASURED — `perf stat -e instructions`, FIXED 2.4M-command cold mix, candidate
(cmdname) vs control (genclock = prior HEAD), 4 rounds:

| round | control instr | candidate instr |
|---|---:|---:|
| 1 | 21,639,590,175 | 21,475,321,153 |
| 2 | 21,649,396,509 | 21,475,093,556 |
| 3 | 21,635,635,904 | 21,471,814,939 |
| 4 | 21,647,441,348 | 21,476,582,455 |

**-168 M instructions (-0.78%), ~70 instr/cmd, all 4 rounds with non-overlapping
bands** (control ~21,643 M ±7 M, candidate ~21,475 M ±2 M). Lever score: **1 win /
0 loss / 0 neutral** (server-CPU). Helps every dispatched command. Cumulative
fr-runtime dispatch reduction this session (genclock + cmdname): ~21,720 M →
21,475 M ≈ **-1.1% instructions/cmd** vs pre-session baseline.

## BlackThrush pubsub empty-map fast-path (MEASURED, hot-path profile, 2026-06-20)

First profile of the PURE GET/SET hot path (3 parallel saturating blasters →
single-threaded server CPU-bound) — distinct from this session's cold-cmd
profiles. It exposed ~11% of GET/SET CPU in **per-command pub/sub bookkeeping**
for a client that never subscribed: `is_pubsub_client` 4.19% + `pubsub_sub_count`
3.40% + (via `effective_output_hard_limit` 2.72%), each doing up to 3 per-client
`HashMap<u64,_>` hash+probes to classify a normal client (redis uses O(1)
`c->flags`).

Fix: O(1) global short-circuit in `is_pubsub_client` / `pubsub_sub_count` /
`pubsub_shard_sub_count` — when the relevant `pubsub_client_*` maps are globally
empty (no client anywhere subscribed = the overwhelmingly common case), no client
can be a subscriber, so return false/0 without the per-client probe. Byte-identical
(empty map yields the same result through the slow path).

Correctness: `cargo test -p fr-runtime --lib pubsub` (13) green; byte-identical by
construction.

MEASURED — `perf stat -e instructions`, FIXED 1.5M-command GET/SET mix (7 GET : 3
SET), candidate (pubsub) vs control (prior HEAD = cmdname), 5 rounds:

| round | control instr | candidate instr |
|---|---:|---:|
| 1 | 2,534,586,349 | 2,466,543,227 |
| 2 | 2,535,692,748 | 2,466,318,022 |
| 3 | 2,535,762,178 | 2,466,733,736 |
| 4 | 2,535,525,933 | 2,466,903,789 |
| 5 | 2,535,377,403 | 2,465,322,380 |

**-69 M instructions (-2.7%), ~46 instr/cmd, all 5 rounds non-overlapping bands**
(control ~2,535.5 M ±0.2 M, candidate ~2,466.4 M ±0.6 M). Lever score: **1 win / 0
loss / 0 neutral** (server-CPU). Biggest relative win this session, on the hottest
commands. Open hot-path follow-ups for CobaltCove (ohsk5, their core): per-command
`effective_output_hard_limit` client-class HashMap lookups, `run_active_expire_cycle`
no-op stats-struct construction (~6.8%).

## BlackThrush GET single keyspace lookup (MEASURED, hot-path, 2026-06-20)

Continuing the pure GET/SET hot-path profile: `get_string_bytes` did THREE
key hash+probes per GET — `record_keyspace_lookup`→`drop_if_expired` probes
`entries` (existence) AND the expiry map (`expiry_ms`), then `entries.get_mut`
probes a third time for the value. Redis's `lookupKeyRead` does it in one.

Fix: when `count_expiring_keys() == 0` (no key in the DB carries a TTL) AND LFU
sampling is off (the default LRU config), a GET can never lazily expire its key
and consumes no RNG, so collapse to a SINGLE `entries.get_mut` that serves both
keyspace hit/miss accounting and the value fetch. Falls to the unchanged
drop_if_expired path when any TTL key exists or LFU is on. Byte-identical: a
TTL-less key cannot evict, so the same hit/miss counter moves with no
eviction/propagation/notification, and LFU-off `touch_access` reads no RNG.

Correctness: differential vs Redis 7.2.4 — **0 diffs** over GET hit/miss/WRONGTYPE,
STRLEN/GETRANGE, **INFO keyspace_hits/misses parity in BOTH the no-TTL fast path
(6/3) and the with-TTL slow path (3/1)**, plus a lazily-expired key (GET nil +
EXISTS 0). fr-store keyspace_hit / lazy_expire unit tests green.

MEASURED — `perf stat -e instructions`, FIXED 1.5M GET/SET (7:3, no TTL keys),
candidate vs control (prior HEAD = pubsub), 6 rounds:

| round | control instr | candidate instr |
|---|---:|---:|
| 1 | 2,471,853,548 | 2,276,674,928 |
| 2 | 2,471,706,213 | 2,277,061,488 |
| 3 | 2,472,030,005 | 2,277,509,363 |
| 4 | 2,471,922,447 | 2,288,981,490 |
| 5 | 2,491,072,620 | 2,297,095,997 |
| 6 | 2,490,174,335 | 2,288,100,404 |

**-195 M instructions (-7.9%), ~130 instr/cmd, all 6 rounds non-overlapping**
(control ≥2,471 M, candidate ≤2,297 M). Lever score: **1 win / 0 loss / 0 neutral**
(server-CPU). Biggest single win this session, on the hottest command (GET).

## BlackThrush fr-persist presorted zset RDB fast path (MEASURED, 2026-06-20)

Fresh cod-a DUMP/reload refresh still shows the zset persistence lane losing to
Redis 7.2.4:

| gate | fr/redis | note |
|---|---:|---|
| `fr-bench dump`, c50 p128 n300k trials=7 | 0.588915x | DUMP path is mostly `fr-store::dump_key`; still a release gap |
| zset-only `DEBUG RELOAD`, 10k zsets x 64 members | 0.308x baseline, 0.451x candidate run | still Redis faster; run-to-run Redis median shifted, so not a clean end-to-end win claim |
| zset-only RESTORE decode half | 0.212x baseline, 0.217x candidate | decode/rebuild remains the larger reload drag |

Kept scoped lever: `fr-persist::encode_compact_zset_listpack` now detects
already-sorted `(member, score)` input and streams directly from the existing
owned member vector instead of allocating borrowed refs and sorting them again.
Runtime RDB snapshots already collect sorted zsets via `iter_asc`; arbitrary
callers still use the old canonical sort path. New guard proves full RDB bytes
match presorted vs shuffled input.

Measured fr-persist encode A/B:

| bench | control | candidate | delta |
|---|---:|---:|---:|
| `cargo bench -p fr-persist --bench rdb_codec -- encode_rdb` | 4.2904 ms | 3.9765 ms | **1.0789x faster** |

Quality gates: focused `fr-persist` zset tests via `rch` passed, `cargo fmt -p
fr-persist --check` passed, `rch cargo check -p fr-persist --all-targets`
passed, `rch cargo clippy -p fr-persist --all-targets -- -D warnings` passed,
and local `cargo test -p fr-conformance -- --nocapture` passed using the
vendored Redis symlink. Lever score: **1 win / 0 loss / 0 neutral** for
fr-persist encode, but release score still carries DUMP/reload risk until
`fr-store::dump_key` and RESTORE decode are attacked.

## BlackThrush ZADD plain-owned store fast path (MEASURED, 2026-06-20)

Fresh write-family refresh against Redis 7.2.4 still had the major losses in
list/set/zset writes: `lpush` 0.80x, `rpush` 0.85x, `sadd` 0.87x, `zadd` 0.73x
from
`artifacts/optimization/frankenredis-bold-verify-coda/20260620T2102Z-current-list-set-zset-refresh/current_vs_redis_p16_c50_n150k_trials7.txt`.

Rejected first pass: the runtime-only plain-ZADD shortcut regressed the target
cell in same-window A/B (`zadd` candidate/control 0.9662x, candidate/Redis
0.6927x, control/Redis 0.7231x), so it was reverted. Artifact:
`artifacts/optimization/frankenredis-bold-verify-coda/20260620T2106Z-zadd-plain-store-candidate/candidate_control_redis_p16_c50_n150k_trials9.txt`.

Kept scoped lever: `Store::zadd_plain_owned` handles flagless ZADD after the
runtime parser already owns member buffers. It skips the generic option engine,
direct-builds single-member sorted sets, de-dupes missing-key multi-member input
without extra member clones, and returns insert-result enums so unchanged scores
avoid write touches.

Measured A/B, same host, fresh control/candidate/Redis processes, P16, c50,
n150k, 9 interleaved trials:

| command | candidate/control | candidate/Redis | control/Redis | verdict |
|---|---:|---:|---:|---|
| zadd | **1.1075x** | 0.8021x | 0.7537x | kept target win |
| sadd | 1.0179x | 0.9268x | 0.8642x | neutral/win guard |
| lpush | 0.9827x | 0.7944x | 0.8218x | neutral guard |
| rpush | 1.0178x | 0.8636x | 0.8471x | neutral/win guard |
| set | 1.0207x | 1.0138x | 1.0438x | neutral/win guard |
| get | 1.0000x | 0.9786x | 0.9613x | neutral guard |
| hset | 0.9932x | 1.0068x | 0.9934x | neutral guard |
| incr | 1.0496x | 1.0208x | 1.0680x | neutral/win guard |

Artifact:
`artifacts/optimization/frankenredis-bold-verify-coda/20260620T2139Z-zadd-plain-owned-store-final/candidate_control_redis_p16_c50_n150k_trials9.txt`.

Quality gates: focused store equivalence test passed; RCH `cargo check -p
fr-store -p fr-runtime --all-targets` passed; `cargo fmt -p fr-store -p
fr-runtime --check` and `git diff --check` passed; RCH `cargo clippy -p
fr-store -p fr-runtime -p fr-server --all-targets -- -D warnings` passed; RCH
`cargo test -p fr-conformance -- --nocapture` passed, including live-oracle
`core_zset` 324/324.

Lever score: **1 win / 1 rejected loss / 0 neutral**. Release status improves
for ZADD but remains below Redis; next high-EV routes are deeper sorted-set
storage/index work plus independent `LPUSH`/`RPUSH` and `SADD` write paths.

## Focused cod-b RANDOMKEY cache-capacity probe (`frankenredis-uhthd`, 2026-06-21)

- Build: no new build; used warm cod-b release binary
  `/data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis`.
- Oracle: vendored Redis 7.2.4.
- Workload: fresh-process 120,000 tiny keys, sample RSS before `RANDOMKEY`, after
  one `RANDOMKEY`, and after a subsequent dirtying `SET`.
- Candidate action: none. The suspected retained-capacity loss was not visible
  in the release RSS gate, so no `shrink_to_fit` source hunk was attempted.

| phase | Redis RSS | FrankenRedis RSS | fr/Redis | verdict |
|---|---:|---:|---:|---|
| before `RANDOMKEY` | `13,291,520` | `32,079,872` | `2.414x` | control gap |
| after `RANDOMKEY` | `13,815,808` | `29,102,080` | `2.106x` | no shrink target |
| after dirtying write | `13,815,808` | `29,126,656` | `2.108x` | no shrink target |

Scorecard impact: **0 wins / 0 source losses / 1 rejected hypothesis**. The next
`uhthd` route remains structural keyspace representation or a deliberate
SCAN/RANDOMKEY semantics tradeoff, not random-key cache capacity trimming.
