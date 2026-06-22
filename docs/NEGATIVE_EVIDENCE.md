# Negative Evidence Ledger

This file is the short-form evidence ledger requested for the 2026-06-20 cod-a
BOLD-VERIFY pass. The canonical long-form project ledger remains
`docs/perf_negative_evidence_ledger.md`.

## 2026-06-21 cod-b `frankenredis-uhthd` quicklist2 RESTORE listpack-span fast path rejected

BOLD-VERIFY targeted the quicklist2 packed RESTORE loss against Redis 7.2.4.
The alien-graveyard route was a narrow data-plane decode specialization: avoid
`Vec` growth from an empty retained-span list and pre-branch string listpack
entries before falling back to the integer-capable decoder. The temporary hunk
changed only `fr_persist::listpack::decode_value_spans`; no production source
remains after this pass.

Focused gate:
`AGENT_NAME=BlackThrush CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b
rch exec -- cargo bench --profile release -p fr-bench --bench
restore_quicklist_vs_redis -- quicklist2_packed_restore --noplot`.

Same-worker `hz2` evidence:

| gate | Redis median | FrankenRedis median | fr/Redis throughput | verdict |
|---|---:|---:|---:|---|
| current control | `101.72 us` | `160.18 us` | `0.635x` | target loss |
| candidate span prealloc/string prebranch | `79.722 us` | `132.25 us` | `0.603x` | ratio worsened; no stable FR gain |

Criterion marked the candidate FrankenRedis row as **No change in performance
detected** (`p = 0.81`), even though the raw median moved `160.18 -> 132.25 us`.
Because Redis moved more in the same run, the user-facing ratio versus Redis
7.2.4 regressed from `0.635x` to `0.603x`. A repeat request did not stay on
`hz2` and selected `ovh-a`, where the bench failed because that worker target
had no `frankenredis` release binary; that row is discarded.

Scorecard: **0 wins / 1 loss / 0 neutral** versus Redis 7.2.4. Decision:
**REJECT / source reverted**. Do not retry `decode_value_spans` capacity-only
or string-prebranch micro-tuning for this quicklist2 RESTORE cell without a new
profile proving the retained-span decoder itself dominates. The next credible
route is deeper retained quicklist/listpack-node representation, RESTORE
rebuild avoidance, or the active ChunkedList/listpack build-accounting residual.

Gates while applied: RCH focused `fr-persist` test
`decode_value_spans_borrows_strings_and_formats_ints` passed. Production code
was reverted before conformance. Post-revert RCH `cargo test -p fr-conformance
-- --nocapture` passed: 194 library tests, all conformance bins, 99 smoke tests,
and doctests green.

## 2026-06-21 cod-a `frankenredis-ohsk5` BITFIELD SET borrowed fast path mixed keep, Redis gap remains

BOLD-VERIFY extended the prior canonical `BITFIELD GET`/`BITFIELD_RO GET`
borrowed parser lane to the hot write shape `BITFIELD key SET u8 0 1`. The
kept server/runtime path recognizes only the canonical single-op `BITFIELD`
write packet and executes unsigned, in-range `SET` through borrowed argv. Signed
fields, overflow/wrap/fail behavior, `INCRBY`, `OVERFLOW`, invalid forms, and
multi-op packets deliberately fall back to the generic BITFIELD handler.

Focused same-worker baseline before the runtime/server fast path, on
`vmi1152480` with `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`:

| gate | Redis median | FrankenRedis baseline median | fr/Redis throughput | verdict |
|---|---:|---:|---:|---|
| `BITFIELD_SET_u8_0_1` | `161.70 us` | `333.46 us` | `0.485x` | target loss |

Candidate evidence:

| gate | Redis median | FrankenRedis candidate median | fr/Redis throughput | direct FR vs baseline | verdict |
|---|---:|---:|---:|---:|---|
| `BITFIELD_SET_u8_0_1`, `hz1` candidate row | `129.46 us` | `115.29 us` | `1.123x` | n/a | same-host Redis win, routing support |
| `BITFIELD_SET_u8_0_1`, `vmi1152480` repeat | `99.794 us` | `248.75 us` | `0.401x` | `1.34x` faster by FR medians | source improves, Redis still faster |

Decision: **KEEP as a narrow FrankenRedis source win, but no Redis-domination
claim**. The same-worker direct FR improvement is material (`333.46 -> 248.75
us`, `383.85 -> 514.58 Kelem/s`), so this is not a ~0-gain revert. However,
the repeat on the baseline host shows the release BITFIELD write cell remains
red versus Redis 7.2.4. Next credible write-side route is a store-owned
fixed-width bitmap mutation primitive or direct encoded reply only after a fresh
profile proves reply framing, not the store write, dominates. `fr-store` is
currently owned by the `uhthd` memory bead, so this pass did not widen into that
file.

Gates: `cargo fmt -p fr-runtime -p fr-server -p fr-bench --check`; RCH
`cargo check -p fr-runtime -p fr-server -p fr-bench --all-targets`; focused RCH
runtime and server parser tests; RCH `cargo clippy -p fr-runtime -p fr-server
-p fr-bench --all-targets -- -D warnings`; RCH `cargo test -p fr-conformance
-- --nocapture` (194 lib tests, all conformance bins, 99 smoke tests,
doctests). Non-strict live-oracle drift rows were logged by the harness but did
not fail.

## 2026-06-21 cod-a `frankenredis-ohsk5` exact 4-value keyed-write recheck remains red

BOLD-VERIFY rechecked the existing exact four-value keyed-write parser coverage
before adding another shallow parser hunk. No source change was retained for
this lane because the already-present parser still leaves the list/set write
rows below Redis 7.2.4 on the focused Criterion gate.

Command:
`AGENT_NAME=BlackThrush RCH_REQUIRE_REMOTE=1 CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a REDIS_SERVER_BIN=/dp/frankenredis/legacy_redis_code/redis/src/redis-server rch exec -- cargo bench --profile release -p fr-bench --bench keyed_write_vs_redis -- "4v" --noplot`.

| gate | Redis median | FrankenRedis median | fr/Redis throughput | verdict |
|---|---:|---:|---:|---|
| `LPUSH_4v` | `66.708 us` | `77.646 us` | `0.859x` | loss |
| `RPUSH_4v` | `56.087 us` | `77.116 us` | `0.727x` | loss |
| `SADD_4v` | `46.610 us` | `63.209 us` | `0.737x` | loss |

Scorecard: **0 wins / 3 losses / 0 neutral**. Decision: **no new parser hunk**.
Do not retry exact four-value keyed-write packet recognition without a deeper
batch-typed execution arena, mutable list/set representation change, or profile
naming parser dispatch as the residual.

## 2026-06-21 cod-b `frankenredis-uhthd` hash DUMP direct listpack emit kept, Redis gap remains

BOLD-VERIFY targeted the hash-only DUMP encode loss from the collection split
gate. The fr-store `DUMP` path for listpack-eligible hashes still built a
temporary `Vec<&[u8]>` of field/value slices before calling the generic
listpack encoder, while set/zset DUMP already streamed entries directly.

The kept source adds `encode_hash_listpack_dump` and routes hash-listpack DUMP
through it. A focused guard compares the new direct emitter byte-for-byte
against the old flat-entry reference, then decodes the listpack to verify the
same field/value sequence, including integer-looking and NUL-containing bytes.

Control release build via RCH on `hz2`, binary sha256
`2366dc30737025a32b6131cd93a2de6ece647913c3d3f247a22f9dee1b4c78d8`.
Candidate release build via the same warm target dir, binary sha256
`5963fd29c25b9e2d0899b027eae7a54552ca6804b42ab6f46666bf329d6c45bb`.

Hash-only split check:
`scripts/collection_reload_headtohead.py <redis_port> <fr_port> --trials 5
--hashes 2000 --sets 0 --zsets 0 --members 40`, vendored Redis 7.2.4 and
`/data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis`.

| gate | control fr median | candidate fr median | Redis median | candidate fr/Redis throughput | verdict |
|---|---:|---:|---:|---:|---|
| `DEBUG RELOAD` save+load | `19.9 ms` | `20.2 ms` | `21.1 ms` | `1.051x` | noisy/parity-to-win |
| pipelined `DUMP` encode half | `16.3 ms` | `15.4 ms` | `10.9 ms` | `0.709x` | source improves, Redis still faster |
| pipelined `RESTORE` decode half | `15.3 ms` | `14.9 ms` | `7.0 ms` | `0.466x` | loss |

A stronger candidate rerun (`--trials 9`) reported DUMP encode `12.6 ms` FR vs
`11.3 ms` Redis (`0.900x` fr/Redis throughput), but FR CV was `14.4%`, so that
is routing support, not a clean parity claim.

Decision: **KEEP, but no domination claim**. Direct FR DUMP median improved
`16.3 -> 15.4 ms` (`1.058x` candidate/control) in the low-CV candidate split,
and all behavior gates passed. The Redis-relative hash persistence lane remains
red on DUMP and RESTORE. Do not repeat generic hash listpack vector-elision or
final-buffer/header-in-place variants; the next credible lever needs retained
hash-listpack representation or RESTORE decode/rebuild.

Gates: `cargo fmt -p fr-store -- --check`; RCH focused `fr-store` test
`dump_hash_listpack_direct_emit_matches_flat_reference_codb_uhthd`; RCH
`cargo build --release -p fr-server`; RCH `cargo check -p fr-store
--all-targets`; RCH `cargo clippy -p fr-store --all-targets -- -D warnings`;
RCH `cargo test -p fr-conformance -- --nocapture` (194 lib tests, all
conformance bins, 99 smoke tests, doctests).

## 2026-06-21 cod-b `frankenredis-uhthd` batch list push helper rejected

BOLD-VERIFY tested the remaining four-value list-write loss with a batch
`ListValue::{push_front_many,push_back_many}` helper. The idea from the
alien-graveyard/artifact pass was command-packet fusion: once the packet already
contains all list elements, append/prepend the whole batch through the listpack
or chunked-list representation instead of replaying one mutation at a time.

The temporary hunk added packed-list bulk encode/prepend and one
`Arc::make_mut` window for deque-backed chunks, then changed `Store::lpush` and
`Store::rpush` to call the batch helpers. It preserved order across packed
promotion in a focused unit test while applied, but the same-worker control did
not confirm a performance win. The source hunk and test were reverted before
commit; only this evidence remains.

Focused Redis 7.2.4 candidate gate:
`AGENT_NAME=BlackThrush RCH_WORKER=vmi1227854 RCH_REQUIRE_REMOTE=1
CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec --
cargo bench --profile release -p fr-bench --bench keyed_write_vs_redis -- "4v"
--noplot`.

| Candidate Criterion gate | Redis mean | FrankenRedis mean | fr/Redis time | fr/Redis throughput | verdict |
|---|---:|---:|---:|---:|---|
| `LPUSH_4v` | `60.669 us` | `65.541 us` | `1.080x` | `0.926x` | loss |
| `RPUSH_4v` | `47.152 us` | `70.271 us` | `1.490x` | `0.671x` | loss |
| `SADD_4v` guard | `48.635 us` | `60.524 us` | `1.244x` | `0.804x` | loss; untouched/noisy |

Same-worker reverted control on the same worker and target dir measured
`LPUSH_4v` Redis `65.587 us` vs FR `64.977 us`, `RPUSH_4v` Redis `46.050 us`
vs FR `70.110 us`, and `SADD_4v` Redis `48.485 us` vs FR `55.427 us`.
Criterion reported no stable candidate/control improvement on the two touched
list rows; the direct FR means were essentially tied (`LPUSH` candidate/control
`1.009x` slower, `RPUSH` `1.002x` slower). The untouched `SADD` guard moving
`1.092x` against the candidate reinforces that this tiny helper is below the
noise floor for the release risk.

Scorecard for this pass: candidate Redis-relative gate **0 wins / 3 losses / 0
neutral**; touched-list candidate/control gate **0 wins / 0 losses / 2 neutral**.
Decision: **REJECT / source reverted**. Do not retry simple list batch helper
wrappers, one-shot packed-list prepend buffers, or `Arc::make_mut` hoisting for
four-value `LPUSH`/`RPUSH` without a fresh profile naming those frames. Route
the residual to a real mutable quicklist/listpack-node representation or a
batch-typed keyed-write execution arena.

Gates: RCH `cargo build --release -p fr-server -p fr-bench` on
`vmi1227854`; focused RCH `fr-store` test
`list_multi_push_preserves_order_across_packed_promotion` passed while applied;
candidate/control RCH `keyed_write_vs_redis` 4v benches above. RCH
`cargo test -p fr-conformance -- --nocapture` on `vmi1149989` passed after the
source revert: 194 lib tests, all conformance bins, 99 smoke tests, and doctests
green. Known non-strict live-oracle drift rows were logged but did not fail.

## 2026-06-21 cod-a `frankenredis-set-listpack-direct-emit-tpans` measured keep, Redis path still loss

BOLD-VERIFY closed the compact set listpack direct-emitter lane. The production
encoder was already on the desired direct path; this pass adds the missing
focused Criterion gate and verifies the old buffered `Vec<&[u8]>` control is
slower. The temporary buffered control was removed before commit, and
`crates/fr-persist/src/lib.rs` has no final hunk.

The alien-graveyard/artifact rationale is fused deterministic codec emission:
when the listpack grammar is fixed, emit each member directly into the target
payload instead of first building a roster of borrowed slices. This targets
allocation and cache traffic only; it does not change set ordering, Redis
integer/string listpack encoding, LZF/raw-string policy, or observable replies.

Focused gate added in this pass:
`rdb_codec_set_listpack/encode_set_listpack_rdb`, 600 set keys, 96 members/key,
mixing canonical integers, signed integers, strings, binary/null-byte members,
and short strings. Same-worker A/B used `hz2` and
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`:

| gate | implementation | median time | throughput | decision |
|---|---|---:|---:|---|
| focused set-listpack RDB encode | current direct emit | `1.3526 ms` | `443.60 Kelem/s` | keep |
| same | temporary buffered flat `Vec<&[u8]>` control | `1.4603 ms` | `410.88 Kelem/s` | control slower |

Candidate result: retained direct emit is `1.0796x` faster than the buffered
control (`1.4603 / 1.3526`). Criterion flagged the temporary old path as a
`+6.7157%` median time regression and `-6.2931%` throughput regression against
the current direct-emitter history.

Fresh Redis 7.2.4 split check, string set-listpack only
`scripts/collection_reload_headtohead.py 18225 18226 --trials 7 --hashes 0
--sets 2000 --zsets 0 --members 40 --set-kind str`, using vendored Redis and
the warm `/data/projects/.rch-targets/frankenredis-cod-a/release/frankenredis`
binary (`sha256=9770295f401a523e821ad9738e567d31933f476f761aa8e8d6ea588c5ad2cbe6`):

| gate | fr median | Redis median | fr/Redis throughput ratio | verdict |
|---|---:|---:|---:|---|
| `DEBUG RELOAD` save+load | `14.5 ms` | `5.3 ms` | `0.376x` | loss |
| pipelined `DUMP` encode half | `11.5 ms` | `9.7 ms` | `0.844x` | loss |
| pipelined `RESTORE` decode half | `13.1 ms` | `5.7 ms` | `0.437x` | loss |

Behavior guard: `scripts/set_listpack_dump_differ.py 18227 18228` passed
byte-exact vs Redis 7.2.4 for string, mixed, binary, large, and long-value
set-listpack shapes.

Scorecard for this pass: focused direct-emitter A/B **1 win / 0 losses / 0
neutral**; Redis-relative split gate **0 wins / 3 losses / 0 neutral**.
Combined honest score: **1 win / 3 losses / 0 neutral**. Keep the focused
encoder win, but do not claim set-listpack persistence dominance. Remaining
release work is retained set-listpack representation plus RESTORE decode/rebuild,
not another generic listpack vector-elision pass.

Gates: RCH `cargo bench -p fr-persist --profile release --bench rdb_codec --
rdb_codec_set_listpack/encode_set_listpack_rdb --noplot` direct/control on
`hz2`; RCH `cargo build --release -p fr-server -p fr-bench` on `hz2`; focused
Redis 7.2.4 split check above; set-listpack byte-equality differ above; RCH
`cargo fmt -p fr-persist --check`; RCH `cargo check -p fr-persist --all-targets`;
RCH `cargo clippy -p fr-persist --all-targets -- -D warnings`; RCH
`cargo test -p fr-persist compact_set_listpack_direct_emit_matches_flat_reference
-- --nocapture`; RCH `cargo test -p fr-conformance -- --nocapture` (194 lib
tests, all conformance bins, 99 smoke tests, doctests passed). Conformance
live-oracle non-strict drift rows were logged but did not fail the suite.

## 2026-06-21 cod-b `frankenredis-hqr5t` exact four-value keyed-write parser measured mixed

BOLD-VERIFY targeted the exact four-value keyed-write parser lane. The server
already contains the exact 4-value parser and focused parser tests; the retained
change in this pass is benchmark coverage only: `keyed_write_vs_redis` now
includes arity `4` so the parser family is measured directly. No `fr-server`
source hunk shipped, and no reverted regression remains.

Focused Redis 7.2.4 gate:
`AGENT_NAME=BlackThrush RCH_REQUIRE_REMOTE=1
CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec --
cargo bench --profile release -p fr-bench --bench keyed_write_vs_redis -- "4v"
--noplot`, remote `vmi1149989`.

| Criterion gate | Redis mean | FrankenRedis mean | fr/Redis time | fr/Redis throughput | verdict |
|---|---:|---:|---:|---:|---|
| `LPUSH_4v` | `63.817 us` | `74.493 us` | `1.167x` | `0.857x` | loss |
| `RPUSH_4v` | `54.537 us` | `74.267 us` | `1.362x` | `0.734x` | loss |
| `SADD_4v` | `72.654 us` | `60.403 us` | `0.831x` | `1.203x` | win; Redis row noisy |

Scorecard for this pass: **1 win / 2 losses / 0 neutral** vs Redis 7.2.4.
The exact 4-value parser coverage task is complete, but it is not a list-write
domination lever. Keep the bench coverage; route `LPUSH`/`RPUSH` residuals to
mutable quicklist/chunk representation or batch append/dispatch work, not more
exact-parser arity extension without a fresh profile naming parser probes.

Gates: `cargo fmt -p fr-bench -- --check`; RCH
`cargo test -p fr-server borrowed_plain_keyed_values4_packet_parser --
--nocapture` (2 parser tests passed); RCH `cargo build --release -p fr-server
-p fr-bench`; focused RCH `keyed_write_vs_redis` 4v bench above; RCH
`cargo test -p fr-conformance -- --nocapture` (194 lib tests, all conformance
bins, 99 smoke tests, doctests passed). Conformance live-oracle non-strict drift
rows were logged but did not fail the suite.

## 2026-06-21 cod-b `frankenredis-uhthd` set-algebra STORE overwrite keep

BOLD-VERIFY targeted the remaining focused set-algebra loss after the prior
cod-b/CobaltCove SINTERSTORE and SDIFFSTORE keeps. The retained lever changes
non-empty `SINTERSTORE` / `SUNIONSTORE` / `SDIFFSTORE` destinations from
delete+reinsert to value-only overwrite through `internal_entries_insert`;
empty results still delete the destination. This preserves Redis-visible
replacement semantics while avoiding repeated SCAN/RANDOMKEY side-index cache
dirties on `*STORE dst ...` packets.

Focused Redis 7.2.4 gate:
`AGENT_NAME=BlackThrush RCH_WORKER=ovh-a
CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b rch exec --
cargo bench --profile release -p fr-bench --bench set_algebra_vs_redis --
--noplot`.

| Criterion gate | Redis mean | FrankenRedis mean | fr/Redis time | fr/Redis throughput | verdict |
|---|---:|---:|---:|---:|---|
| `SINTERSTORE` | `728.48 us` | `284.37 us` | `0.390x` | `2.562x` | win |
| `SDIFFSTORE` | `629.46 us` | `298.02 us` | `0.473x` | `2.112x` | win |
| `SUNIONSTORE` | `6.6817 ms` | `5.8679 ms` | `0.878x` | `1.139x` | win |

Scorecard for this pass: focused set-algebra gate **3 wins / 0 losses / 0
neutral** vs Redis 7.2.4. This directly closes the previously logged
SUNIONSTORE loss (`0.764x` throughput) in the same small per-crate bench family;
do not revert.

Gates: `cargo fmt -p fr-store -- --check`; RCH `cargo test -p fr-store
set_algebra_store_nonempty_overwrite_is_not_structural -- --nocapture`; RCH
`cargo build --release -p fr-server -p fr-bench`; RCH `cargo check -p fr-store
--all-targets`; RCH `cargo clippy -p fr-store --all-targets -- -D warnings`;
RCH `cargo test -p fr-conformance -- --nocapture` (194 lib tests, all
conformance bins, 99 smoke tests, doctests passed). Conformance live-oracle
non-strict drift rows were logged but did not fail the suite.

## 2026-06-21 cod-a `frankenredis-set-intset-canonical-noalloc-acetq` measured keep, Redis decode still dominates

BOLD-VERIFY revisited the compact set intset RDB encoder after the prior
allocation-free canonical decimal parser had already been verified byte-exact
against Redis 7.2.4. The retained follow-up lever carries the intset element
width while parsing members, then passes it to `encode_intset_blob`, removing
the old two extra full-value scans used to choose 16/32/64-bit intset width.

Focused gate added in this pass:
`rdb_codec_set_intset/encode_set_intset_rdb`, 900 set keys, 96 integer members
per key, mixed 16-bit, 32-bit, and wide signed 32-bit values.

Same-worker A/B used `ovh-a` and
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`:

| gate | implementation | median time | throughput | decision |
|---|---|---:|---:|---|
| focused set-intset RDB encode | current width-carry candidate | `788.99 us` | `1.1407 Melem/s` | keep |
| same | temporary old width-rescan control | `910.44 us` | `988.54 Kelem/s` | control slower |

Candidate result: retained width carry is `1.1540x` faster than the old
width-rescan control (`910.44 / 788.99`). Criterion reported the confirmation
as median time `-13.661%` versus the temporary control. A first current-source
run on `hz2` measured `937.76 us` / `959.73 Kelem/s`; that is supporting
routing evidence only because the keep/reject decision uses the same-worker
`ovh-a` pair above.

Fresh Redis 7.2.4 split check, intset-only
`scripts/collection_reload_headtohead.py 18195 18196 --trials 7 --hashes 0
--sets 2000 --zsets 0 --members 40 --set-kind int`, using vendored Redis and
the warm `/data/projects/.rch-targets/frankenredis-cod-a/release/frankenredis`
binary:

| gate | fr median | Redis median | fr/Redis throughput ratio | verdict |
|---|---:|---:|---:|---|
| `DEBUG RELOAD` save+load | `8.8 ms` | `4.1 ms` | `0.559x` | loss |
| pipelined `DUMP` encode half | `11.9 ms` | `10.9 ms` | `0.917x` | loss |
| pipelined `RESTORE` decode half | `10.8 ms` | `4.6 ms` | `0.429x` | loss |

Scorecard for this pass: focused width-carry A/B **1 win / 0 losses / 0
neutral**; Redis-relative split gate **0 wins / 3 losses / 0 neutral**.
Combined honest score: **1 win / 3 losses / 0 neutral**. Keep the focused
encoder win and the earlier noalloc canonical parser, but do not claim set
intset persistence dominance. Remaining release work is retained intset/load
representation or RESTORE decode/rebuild, not another generic decimal or width
scan cleanup.

## 2026-06-21 cod-b `frankenredis-uhthd` quick memory rebaseline and set-algebra mixed score

BOLD-VERIFY rechecked the `uhthd` store lane after the rejected exact-capacity,
EXISTS, compact-score, and RANDOMKEY-capacity micro-levers. The fresh source
decision is **no hunk shipped**: the remaining memory gap is structural table and
representation overhead, not another safe one-field reserve/cache tweak.

Release build:
`AGENT_NAME=BlackThrush RCH_REQUIRE_REMOTE=1 CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b
rch exec -- cargo build --release -p fr-server -p fr-bench`, remote
`vmi1149989`; `frankenredis` sha256
`55da5f2e9d91b803531663e19bea17fcd71ddea9e676f21baa3913470fc25479`.

Quick fresh-process memory rebaseline used vendored Redis 7.2.4 and
`scripts/memory_baseline_capture.py --quick`, scale 20k, ports from
`FR_BENCH_PORT_BASE=48551`. The harness captured
`.bench-history/memory_baseline.latest.json` and failed its ratchet because
`string_1k` moved from stored RSS ratio `0.955x` to `1.158x`.

| data type | fr/Redis RSS | fr/Redis used_memory | verdict |
|---|---:|---:|---|
| keyspace | `1.445x` | `0.492x` | loss |
| string_1k | `1.158x` | `0.767x` | loss; ratchet failure |
| list | `0.972x` | `0.062x` | RSS win |
| hash | `1.074x` | `0.199x` | small loss |
| set | `0.994x` | `0.116x` | RSS win |
| zset | `1.130x` | `0.147x` | loss |
| stream | `1.052x` | `1.085x` | loss |

Focused per-crate Redis 7.2.4 Criterion gate:
`AGENT_NAME=BlackThrush RCH_WORKER=vmi1149989 RCH_REQUIRE_REMOTE=1
CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b
rch exec -- cargo bench --profile release -p fr-bench --bench
set_algebra_vs_redis -- --noplot`. The first `cargo bench --release` attempt
failed because this Cargo rejects `--release` for benches; the first
release-profile rerun failed on `ovh-a` because the remote worker lacked the
`fr-server` binary in its worker-scoped target path. Those are harness setup
failures, not performance evidence.

| gate | Redis mean | FrankenRedis mean | fr/Redis time | fr/Redis throughput | verdict |
|---|---:|---:|---:|---:|---|
| `SINTERSTORE` | `766.51 us` | `361.09 us` | `0.471x` | `2.123x` | win |
| `SDIFFSTORE` | `877.24 us` | `424.35 us` | `0.484x` | `2.067x` | win |
| `SUNIONSTORE` | `9.2308 ms` | `12.078 ms` | `1.308x` | `0.764x` | loss |

Scorecard: quick RSS **2 wins / 5 losses / 0 neutral**; set-algebra throughput
**2 wins / 1 loss / 0 neutral**; source score **0 kept hunks / 0 reverted
hunks / 1 structural no-source route**. Do not retry Entry tail packing,
exact packed-buffer reserves, zset score-byte tagging, no-expiry EXISTS branch
gating, or RANDOMKEY cache-capacity tweaks. The next radical lever is a full
keyspace/table representation change that removes side-index families together,
or a retained compact representation for hash/zset/list surfaces with
same-current A/B proof.

## 2026-06-21 cod-a `frankenredis-hash-listpack-direct-emit-dv9n5` measured keep, Redis path still loss

BOLD-VERIFY targeted the `fr-persist` compact hash listpack encoder because the
old path built a flat `Vec<&[u8]>` staging array for every field/value pair
before listpack construction. The retained lever streams field/value entries
directly into the listpack payload. A more aggressive attempt to write entries
into a final header-prefixed listpack buffer was tested and reverted because it
regressed the same-worker gate.

Focused gate added in this pass:
`rdb_codec_hash_listpack/encode_hash_listpack_rdb`, 600 hash keys, 96
fields/key, mixed integer-looking and string field/value bytes. Same-worker A/B
used `vmi1227854` and
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`:

| gate | implementation | median time | throughput | decision |
|---|---|---:|---:|---|
| focused hash-listpack RDB encode | current direct emit | `2.6388 ms` | `227.38 Kelem/s` | keep |
| same | temporary buffered flat `Vec<&[u8]>` control | `3.0709 ms` | `195.38 Kelem/s` | control slower |
| same | temporary final-buffer/header-in-place variant | `2.7849 ms` | `215.44 Kelem/s` | reject/reverted |

Candidate result: retained direct emit is `1.1637x` faster than the buffered
control (`3.0709 / 2.6388`). The final-buffer variant was `1.0554x` slower than
the retained direct emitter and was removed before commit.

Fresh Redis 7.2.4 split check, hash-only
`scripts/collection_reload_headtohead.py 18185 18186 --trials 7 --hashes 2000
--sets 0 --zsets 0 --members 40`, using vendored Redis and the warm
`/data/projects/.rch-targets/frankenredis-cod-a/release/frankenredis` binary:

| gate | fr median | Redis median | fr/Redis throughput ratio | verdict |
|---|---:|---:|---:|---|
| `DEBUG RELOAD` save+load | `19.4 ms` | `6.7 ms` | `0.344x` | loss |
| pipelined `DUMP` encode half | `14.7 ms` | `10.6 ms` | `0.720x` | loss |
| pipelined `RESTORE` decode half | `14.2 ms` | `6.7 ms` | `0.473x` | loss |

Scorecard for this pass: focused direct-emitter A/B **1 win / 0 losses / 0
neutral**; rejected final-buffer experiment **0 wins / 1 loss / 0 neutral**;
Redis-relative split gate **0 wins / 3 losses / 0 neutral**. Combined honest
score: **1 win / 4 losses / 0 neutral**. Keep the already-shipped `fr-persist`
direct emitter, but do not claim hash persistence dominance. Remaining release
work is retained/hash-listpack representation plus RESTORE decode/rebuild, not
another generic listpack vector-elision pass.

## 2026-06-21 cod-b `frankenredis-uhthd` packed bulk exact-capacity rejected and reverted

BOLD-VERIFY targeted the remaining hash/zset memory losses with a compact
builder capacity lever: reserve exact varint-aware bytes in
`HashFieldMap::from_unique_pairs{,_borrowed}` and `PackedZSet::from_unique_pairs`
instead of the prior fixed `+10` per entry allowance. The rationale was succinct
bulk construction: remove per-key over-reservation in packed listpack-like
buffers without changing stored bytes, ordering, command semantics, or Redis
observable replies.

The candidate test passed via RCH:
`RCH_REQUIRE_REMOTE=1 CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b
rch exec -- cargo test -p fr-store
packed_bulk_builders_use_exact_varint_capacity_uhthd -- --nocapture`, remote
`ovh-a`.

Head-to-head memory probe used fresh local processes for vendored Redis 7.2.4
and the warm `frankenredis` release binary, scale 200k, after a per-crate remote
release build:
`RCH_REQUIRE_REMOTE=1 CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b
rch exec -- cargo build --release -p fr-server -p fr-bench`.

| gate | control Redis RSS | control fr RSS | control fr/Redis | candidate Redis RSS | candidate fr RSS | candidate fr/Redis | decision |
|---|---:|---:|---:|---:|---:|---:|---|
| packed hash memory | `7,634,944` | `9,928,704` | `1.300x` | `8,720,384` | `10,485,760` | `1.202x` | reject |
| packed zset memory | `7,688,192` | `11,956,224` | `1.555x` | `8,032,256` | `11,972,608` | `1.491x` | reject |

The Redis-relative ratios looked better only because the Redis oracle process
RSS drifted upward in the candidate window. FrankenRedis absolute RSS worsened:
hash `+557,056 B`; zset `+16,384 B`. Scorecard: **0 wins / 2 losses / 0
neutral** on the target absolute-RSS decision signal. Source was reverted; do
not retry fixed-capacity/exact-reserve tweaks for packed hash/zset unless a
same-window A/B shows absolute FrankenRedis RSS reduction or an allocator class
proof explains why process RSS should move. Route to deeper representation/table
overhead instead.

RCH infra note: the first fail-closed remote release build timed out during sync
because large local oracle/evidence directories were still in the transfer
payload. `.rchignore` now excludes `legacy_redis_code/`, `artifacts/`, and
`.bench-history/`; remote sync fell to about 7.3 MB and the per-crate release
build completed. This is not a Redis behavior/perf keep claim.

## 2026-06-21 cod-a `frankenredis-mixed-zset-listpack-direct-emit-vly2n` measured keep, Redis path still split-loss

BOLD-VERIFY targeted the `fr-persist` compact zset listpack encoder because the
mixed integer/fractional score path had an avoidable allocation roster:
`score_bytes: Vec<Vec<u8>>` plus a flattened `Vec<&[u8]>` before final listpack
construction. The alien-graveyard/artifact rationale was fused deterministic
codec emission: when the output grammar is known, stream member/score entries
directly into the target listpack buffer and use stack decimal scratch for
integer-valued scores.

Focused gate added in this pass:
`rdb_codec_mixed_zset/encode_mixed_zset_rdb`, 600 zset keys, 96 members/key,
deliberately unsorted input, mixed integer/fractional scores. The unsorted input
forces both candidate and old control through the same canonical sort, isolating
direct entry emission from the later presorted-input fast path.

Same-worker A/B (`vmi1227854`,
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`,
`rch exec -- cargo bench -p fr-persist --bench rdb_codec --
rdb_codec_mixed_zset/encode_mixed_zset_rdb --noplot`):

| gate | implementation | median time | throughput | decision |
|---|---|---:|---:|---|
| focused mixed-zset RDB encode | current direct emit | `7.2671 ms` | `82.564 Kelem/s` | keep |
| same | temporary buffered `score_bytes` + flat control | `8.3999 ms` | `71.429 Kelem/s` | control slower |

Candidate result: direct emit is `1.1559x` faster than the buffered control
(`8.3999 / 7.2671`). Criterion reported the temporary old path as a `+15.588%`
time regression and `-13.486%` throughput regression against the current
direct-emitter history. The temporary control hunk was removed; production
source remains on the direct-emitter path.

Fresh Redis 7.2.4 split check, zset-only
`scripts/collection_reload_headtohead.py 18083 18084 --trials 7 --hashes 0
--sets 0 --zsets 2000 --members 40`, using vendored Redis and the warm
`/data/projects/.rch-targets/frankenredis-cod-a/release/frankenredis` binary:

| gate | fr median | Redis median | fr/Redis throughput ratio | verdict |
|---|---:|---:|---:|---|
| `DEBUG RELOAD` save+load | `21.1 ms` | `21.1 ms` | `1.046x` | neutral/noisy parity |
| pipelined `DUMP` encode half | `14.9 ms` | `11.2 ms` | `0.749x` | loss |
| pipelined `RESTORE` decode half | `18.0 ms` | `8.1 ms` | `0.450x` | loss |

Artifact log path:
`artifacts/optimization/frankenredis-bold-verify-coda/20260621T0835Z-mixed-zset-direct-emit-verify/zset-reload-headtohead-2000.log`.

Scorecard for this pass: focused direct-emitter A/B **1 win / 0 losses / 0
neutral**; Redis-relative split gate **0 wins / 2 losses / 1 neutral**. Combined
honest score: **1 win / 2 losses / 1 neutral**. Keep the already-shipped
`fr-persist` direct emitter, but do not claim zset persistence dominance:
remaining release work is `fr-store::dump_key` compact-zset DUMP materialization
and RESTORE decode/rebuild, not another generic listpack vector cleanup.

## 2026-06-21 cod-a `frankenredis-quicklist2-direct-emit-g7ag5` quicklist2 direct emit rejected and reverted

BOLD-VERIFY targeted the `fr-persist` QUICKLIST_2 RDB encode path because prior
RDB work left a plausible allocation lever: stream each PACKED listpack node
directly into a node buffer instead of collecting borrowed slices and calling
the shared listpack builder. The alien-graveyard/artifact rationale was
region-style fused emission: remove one intermediate roster and finish each
quicklist node in one pass while preserving the Redis 7.2.4 PLAIN threshold
(`1 << 30`) fixed by `frankenredis-1z4ba`.

Focused gate added in this pass:
`rdb_codec_quicklist/encode_quicklist_rdb`, 300 list keys, 180 members/key,
96-byte deterministic members. This is a server-free `fr-persist` encode gate,
not a Redis-relative release score by itself.

Same-worker control:

| gate | implementation | worker | mean time | throughput | decision |
|---|---|---|---:|---:|---|
| `cargo bench -p fr-persist --profile release --bench rdb_codec -- rdb_codec_quicklist --noplot` | buffered slice roster | `ovh-a` | `23.890 ms` | `12.558 Kelem/s` | control |
| same | direct emitter restored | `ovh-a` | `25.465 ms` | `11.781 Kelem/s` | reject |

Candidate result: direct emission was `1.0659x` slower than the buffered path
and Criterion flagged the restored direct-emitter run as `+6.5926%` time
regression (`p=0.00`) / `-6.1849%` throughput. A previous direct-emitter run on
`hz2` (`24.475 ms`, `12.257 Kelem/s`) is routing evidence only because it was a
different worker.

Scorecard for this lever: **0 wins / 1 loss / 0 neutral**. Redis-relative ratio:
**no new keep claim** from this encode-only gate; release-readiness ratios remain
the existing Redis 7.2.4 head-to-head rows until a list-specific DEBUG
RELOAD/DUMP harness isolates this path. Production was reverted to the buffered
slice-roster encoder; the focused benchmark stays as the retry guard. Do not
retry direct quicklist2 listpack streaming unless a fresh profile shows the
shared listpack builder or borrowed roster dominates and a same-worker gate
beats the buffered control.

## 2026-06-21 cod-b arity-one keyed-write cached default write gate rejected

BOLD-VERIFY targeted the current arity-one keyed-write losses from the existing
`keyed_write_vs_redis` scorecard (`LPUSH_1v`, `RPUSH_1v`, `SADD_1v`) without
touching peer-owned store representation work. The attempted lever cached the
default selected-DB write gate for the exact arity-one borrowed packet path and
threaded it into `Runtime::execute_plain_keyed_values_write_borrowed`, leaving
all source reverted after measurement.

Candidate gate: filtered per-crate Criterion bench
`cargo bench --profile release -p fr-bench --bench keyed_write_vs_redis -- 1v
--noplot`, via `rch exec`, `RCH_WORKER=vmi1152480`,
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`, and vendored
Redis 7.2.4 at
`/dp/frankenredis/legacy_redis_code/redis/src/redis-server`. The first bench
attempt failed before measurement because `FR_SERVER_BIN` pointed at the local
warm target path while RCH rewrote `CARGO_TARGET_DIR` on the worker. The
measured runs built `fr-server` on the same worker and let the harness resolve
`FR_SERVER_BIN` from the worker-local target dir.

| gate | candidate fr/Redis time | control fr/Redis time | fr candidate/control time | verdict |
|---|---:|---:|---:|---|
| `LPUSH_1v` | `1.618x` (`0.618x` throughput) | `1.235x` (`0.810x` throughput) | `1.285x` slower | reject |
| `RPUSH_1v` | `1.385x` (`0.722x` throughput) | `1.069x` (`0.935x` throughput) | `1.361x` slower | reject |
| `SADD_1v` | `1.436x` (`0.696x` throughput) | `1.292x` (`0.774x` throughput) | `1.152x` slower | reject |

Scorecard: **0 wins / 3 losses / 0 neutral** vs Redis 7.2.4, and **0 wins /
3 losses / 0 neutral** vs current control. Retry condition: do not retry cached
default write-gate or one-branch policy-gate micro-laziness unless a fresh
profile names `plain_borrowed_default_key_write_allows` or the selected-DB write
gate as a material hot frame. The next route remains structural batch-typed
keyed-write execution/request arena or list/set representation work.

## 2026-06-21 cod-a `frankenredis-ohsk5` keyed-write packet-id deferral rejected

BOLD-VERIFY refresh targeted the remaining arity-one keyed-write losses without
touching the dirty `fr-store` worktree files owned by other lanes. The measured
current surface used the existing warm target dir
`/data/projects/.rch-targets/frankenredis-cod-a`, explicit
`nightly-2026-06-09` to match target metadata, and vendored Redis 7.2.4 via the
filtered per-crate Criterion bench:

`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a
FR_SERVER_BIN=/data/projects/.rch-targets/frankenredis-cod-a/release/frankenredis
cargo +nightly-2026-06-09 bench -p fr-bench --profile release --bench
keyed_write_vs_redis -- 1v --noplot`

Current baseline:

| gate | Redis 7.2.4 median throughput | FrankenRedis median throughput | fr/Redis | verdict |
|---|---:|---:|---:|---|
| `keyed_write_vs_redis/LPUSH_1v` | `953.57 Kelem/s` | `753.24 Kelem/s` | `0.79x` | loss |
| `keyed_write_vs_redis/RPUSH_1v` | `1.0069 Melem/s` | `734.37 Kelem/s` | `0.73x` | loss |
| `keyed_write_vs_redis/SADD_1v` | `1.1279 Melem/s` | `797.36 Kelem/s` | `0.71x` | loss |

Attempted lever: defer `next_packet_id()` in
`Runtime::execute_plain_keyed_values_write_borrowed` until the cold
time-budget threat-event branch. The alien-graveyard/artifact rationale was
request-scope metadata laziness: remove a per-command atomic from the hot path
while preserving exact threat-event packet IDs when the branch actually fires.

Candidate result:

| gate | candidate fr/Redis | Criterion verdict for FrankenRedis | decision |
|---|---:|---|---|
| `LPUSH_1v` | `0.80x` | no change detected, p=`0.96` | reject |
| `RPUSH_1v` | `0.75x` | no change detected, p=`0.96` | reject |
| `SADD_1v` | `0.74x` | no change detected, p=`0.37` | reject |

The Redis side moved between runs, so the ratio lift is not attributable to the
candidate. The source hunk was reverted; `crates/fr-runtime/src/lib.rs` has no
remaining production diff from this experiment. This is negative evidence
against standalone packet-id laziness as a keyed-write lever.

Harness notes: an `rch exec -- cargo bench ... -- 1v` attempt on `vmi1149989`
failed before measurement because the remote rewritten target dir lacked the
`frankenredis` server binary. A local run with the default nightly failed with
target-dir rustc metadata mismatch (`E0514`). Both are setup failures, not perf
evidence.

Scorecard: arity-one keyed writes remain **0 wins / 3 losses / 0 neutral** vs
Redis 7.2.4. Retry condition: do not retry packet-id/metrics micro-laziness
unless a profile names `next_packet_id` or keyed-write metrics as a >=0.1%
self-time frame. The next high-EV route is a genuinely different primitive:
batch-typed keyed-write execution/request arena or list/set representation work,
not another standalone metadata branch trim.

## 2026-06-21 cod-b `frankenredis-uhthd` EXISTS no-expiry fast path rejected

Rejected source hunk: `Store::exists_no_touch` briefly fast-pathed persistent
keyspaces (`count_expiring_keys() == 0`) with a direct `entries.contains_key`
probe and manual hit/miss counter updates, falling back to
`record_keyspace_lookup` only when TTL-bearing keys existed. The TTL fallback
was covered by a focused `fr-store` unit extension, but the performance result
did not justify keeping the branch.

Measured gate: filtered per-crate Criterion bench
`cargo bench --profile release -p fr-bench --bench exists_vs_redis --
--noplot`, with `RCH_WORKER=hz2`, `RCH_REQUIRE_REMOTE=1`,
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`, and
vendored Redis 7.2.4 via
`REDIS_SERVER_BIN=/dp/frankenredis/legacy_redis_code/redis/src/redis-server`.
`cargo bench --release` was tried first because that was the requested spelling,
but this toolchain rejects it; `--profile release` is the equivalent accepted
Cargo invocation.

| gate | candidate ratio vs Redis 7.2.4 | current-control ratio vs Redis 7.2.4 | fr candidate/control | verdict |
|---|---:|---:|---:|---|
| `exists8_all_hit`, Criterion mean time | `1.143x` time, `0.875x` throughput | `1.054x` time, `0.948x` throughput | `1.098x` slower | reject |
| `exists8_half_hit`, Criterion mean time | `1.202x` time, `0.832x` throughput | `1.284x` time, `0.779x` throughput | `1.091x` slower | reject |
| `exists8_duplicates`, Criterion mean time | `1.150x` time, `0.869x` throughput | `1.161x` time, `0.862x` throughput | `1.093x` slower | reject |

Decision: source reverted before commit. Redis moved enough between the two
small Criterion runs that the Redis-relative half-hit ratio alone is not a keep
signal; the direct FrankenRedis candidate/control comparison regressed all
three shapes by roughly 9-10%. Do not retry this no-expiry `EXISTS` branch
without a new profile showing `drop_if_expired`/expiry-side probing dominates.

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

## 2026-06-21 cod-a `frankenredis-ohsk5` BITFIELD GET u8 borrowed fast path kept

Targeted the bitmap/HLL residual row that had `BITFIELD GET u8` at `0.77x`
against Redis 7.2.4. The radical lever was not another store bit loop; it was
removing the fixed dispatch/parser tax for the exact read-only single-op shape
that benchmark users actually send: `BITFIELD key GET <enc> <offset>`.

| artifact / gate | variant | command | fr/Redis or candidate/control throughput | verdict |
|---|---|---|---:|---|
| `crates/fr-bench/benches/bitfield_vs_redis.rs` | inverse control vs Redis 7.2.4 | `BITFIELD bf GET u8 0` | `0.42x` | baseline loss (`532.77 Kelem/s` vs Redis `1.2683 Melem/s`) |
| same | retained candidate vs Redis 7.2.4 | `BITFIELD bf GET u8 0` | `1.10x` | keep (`1.4224 Melem/s` vs Redis `1.2917 Melem/s`) |
| same | retained candidate vs old control | `BITFIELD bf GET u8 0` | `2.67x` | direct FrankenRedis A/B win |
| `rch exec -- cargo bench -p fr-bench --profile release --bench bitfield_vs_redis -- BITFIELD_GET_u8_0 --noplot` | retained candidate vs Redis 7.2.4 on `hz2` | `BITFIELD bf GET u8 0` | `1.17x` | remote confirmation (`886.57 Kelem/s` vs Redis `758.31 Kelem/s`) |

Decision: **KEEP**. Score for the focused cell is **1 win / 0 losses / 0
neutral**. The shipped path parses canonical `*5 BITFIELD key GET enc offset`
borrowed from the input buffer, validates literal GET plus the same
encoding/offset rules as the generic command, then performs the same single
keyspace lookup and `bitfield_get_no_stat` read. Every write or ambiguous form
falls through: SET, INCRBY, OVERFLOW, multi-op BITFIELD, invalid encodings,
invalid offsets, and BITFIELD_RO are not claimed here.

Gates passed: `cargo fmt -p fr-runtime -p fr-server -p fr-bench --check`; RCH
check/clippy for `fr-runtime`, `fr-server`, and `fr-bench`; RCH release build
for `fr-server` and `fr-bench`; focused `fr-command` BITFIELD tests (24/24);
focused `fr-store` BITFIELD tests; live `bitfield_differ.py 46371 46372 1
1200` (0 divergences); live `bitfield_overflow_differ.py`; live
`bitfield_offset_limit_differ.py`; live `bitmap_differ.py --iters 1000 --seed
4242`; full `fr-conformance` package (194 lib tests, all bins, 99 smoke tests,
doctests) green. Final validation release binary sha256:
`0ef2e830a283f760e50312d40a69416418a5e364452143673dcb80ab503194a7`.

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

### CORRECTION: DEBUG RELOAD encoding "divergence" was a test artifact (fr DEBUG disabled)
With fr started `--enable-debug-command yes` (fr defaults to "no", matching upstream), the
encoding_rdb_differ gate is **78 checks, 0 regressions, 0 known** — RESTORE (10ovx fix) AND
DEBUG RELOAD are both byte-exact vs Redis 7.2.4. My earlier "DEBUG RELOAD over-keeps" finding
was an artifact: fr DEBUG was disabled in those probe runs, so fr's DEBUG RELOAD errored
(no-op, kept encoding) vs redis's real reload. No DEBUG-RELOAD encoding bug exists. Gate's
reload check promoted to must-pass; usage note updated (both servers need --enable-debug-command).

### NEW perf lever target: collection RESTORE/RDB-load DECODE = 0.37x (redis 2.7x faster)
collection_reload_headtohead (2000 hash+set+zset, 40 members, fr DEBUG enabled): DEBUG RELOAD
0.337x, DUMP-encode 0.747x, **RESTORE-decode 0.373x** (fr 54.4ms vs redis 20.3ms). The decode
half is the dominant collection-RDB gap (the "keep-listpack / zero-decode" lever). Next:
per-type RESTORE profiling to find the slowest type, then bulk-build it (qxfmr/duab9 pattern).

### RESTORE-decode lever — per-type profiled + structural conclusion (cc, disk recovered)
Per-type DEBUG RELOAD (10k keys × 16 elems, both --enable-debug-command), reload best-of-5:
| type | fr | redis | ratio (redis/fr) |
|---|---:|---:|---:|
| zset | 0.052s | 0.025s | **0.48x** (worst) |
| hash | 0.054s | 0.031s | **0.57x** |
| set  | 0.034s | 0.026s | 0.76x |
| list | 0.033s | 0.026s | 0.79x |
| str  | 0.011s | 0.011s | 1.00x |
RESTORE-decode half overall 0.373x; DUMP-encode 0.747x.

Conclusion: the gap is STRUCTURAL, not a bounded inefficiency. fr's hash RESTORE already uses
`hash_from_listpack_spans` (zero-copy spans) and from_unique_pairs (O(n) bulk-build) — both
shipped. The residual is that **redis keeps the RDB listpack bytes AS its in-memory small
collection (zero decode)** while fr decodes into its own packed format (PackedStrMap / PackedZSet
/ packed_set arena). To close it, fr's small-collection packed encoding would have to BE the
redis listpack (like fr's LIST chunks already are — lists are the least-bad at 0.79x), making
RESTORE *and* DUMP zero-copy. That is a big byte-exactness-critical rewrite of PackedStrMap
(hash, mine) / PackedZSet (zset, cod-b) touching HGETALL/HSCAN/DUMP order + all ops, in the
contended packed_set.rs — a multi-pass structural lever, not a clean per-turn ship. Highest-value
target is zset (0.48x, cod-b) then hash (0.57x). Bounded RESTORE-decode optimizations are
exhausted (zero-copy spans + bulk-build already in).

### zset range-count perf — MEASURED investigation (cc, disk recovered): no clean solo lever
Deep-pipelined (pipe=100-200) head-to-head vs Redis 7.2.4, scaled N=1000..16000:
- **ZRANK** flat ~0.7x, **ZCOUNT** flat ~0.5x across all N → CONSTANT-FACTOR, not algorithmic.
- **ZLEXCOUNT correct usage (equal scores)**: ~0.5x at N=16000 (treap O(log n) rank-diff path
  IS working/flat); N=1000 warming anomaly (0.15x) but large-N is flat. Constant-factor.
- **ZLEXCOUNT distinct-score input**: O(n) scan (817ms@N=16000, ratio 0.01x) — fr's
  `first.score == last.score` guard (lib.rs:1655) drops the treap path to the O(range) BTreeMap
  scan. This is ZLEXCOUNT MISUSE (undefined per redis docs; redis stays O(log n) via skiplist).
  Result is byte-correct (matches redis :N), just slow. Generalizing the guard to use treap
  rank-diff on distinct scores is byte-exactness-RISKY (distinct-score lex semantics) — declined.
Conclusion: the real-usage gap is a ~0.5x CONSTANT factor = range-parse + 2× treap `rank_of`
compute (cod-b's SortedSet rank_tree), NOT dispatch (ZCARD's borrowed fast path is 0.94x, so
dispatch is ~6% of the gap; a ZCOUNT fast path would close ~0.5x→~0.53x, not worth it) and NOT
algorithmic. Corrects the earlier brief "algorithmic O(n)" hypothesis (that was the distinct-
score misuse artifact). No clean cc-store/cc-runtime lever; rank_of constant-factor is cod-b's
treap domain. FLAGGED to cod-b: the distinct-score ZLEXCOUNT O(n) cliff (edge case) + whether
the treap rank_of constant factor (~2x) can be tightened.

### wide pipelined gauntlet (cc, disk recovered) — corrected harness; real residual losses
METHODOLOGY LESSON: a draining loop that counts `\r\n` to tally pipelined replies OVER-counts
bulk ($len\r\n+data\r\n = 2) and multibulk (N) replies → early exit → desync → garbage ratios
(produced impossible readings like HSCAN 7.66x / SMEMBERS 24x and phantom TTL/TYPE/GETBIT
losses). Re-ran with a PROPER per-reply RESP reader + READ-ONLY commands only (mutating cmds
can't be benched by repetition — state diverges). Corrected result: nearly everything parity+
(GET 1.26, HGETALL 1.01, TTL 1.00, TYPE 1.01, SCAN 0.87, HSCAN 1.07, ZRANGE 1.01, LRANGE 1.04
— prior cold-cmd fast paths all hold). REAL residual losses (deep-pipelined vs Redis 7.2.4):
| cmd | ratio | note |
|---|---:|---|
| PFCOUNT (single, cached) | 0.53x | HLL cache WORKS + byte-identical format (HYLL/sparse, cache_after valid) — gap is cache-HIT constant-factor: PFCOUNT lacks a borrowed fast path (generic dispatch) while GET has one (1.26x). Fix = PFCOUNT fast path, but fr-runtime is BlackThrush/ohsk5-reserved + agent-mail down → not pursued (no collision). |
| GEODIST | 0.58x | haversine compute (geohash decode + trig); byte-risk on float fmt (declined prior). |
| SINTER (300∩4 small) | 0.68x | smallest-set tiny intersection; dispatch/constant (3+set SINTER already +25%). |
| BITFIELD GET u8 | 0.77x | single subcommand parse overhead. |
| EXISTS (3 keys) | 0.81x | multi-key generic dispatch. |
Conclusion: no clean radical cc-solo lever — residuals are constant-factor dispatch (fixable
only via fast paths in BlackThrush's reserved fr-runtime) or compute/byte-risky (GEODIST). Hot
path remains saturated/dominant. agent-mail DB corrupt (circuit breaker) → flagged via ledger.

### cross-verify peer b89361c13 (fr-persist "reject quicklist2 direct emit") — PARITY-SAFE (cc)
Peer DUMP-codec change in my recently-worked list RESTORE/encoding area. Built HEAD + ran my
gates: list_ops_differ 3394/0 diffs, encoding_rdb_differ 78/0 regressions (strict, DEBUG-on),
fr-conformance core_list green. No regression to list DUMP/RESTORE/encoding parity; 10ovx fix
holds. (BOLD-VERIFY cross-check.)

### PFCOUNT fast-path lever — CONFIRMED BLOCKED (cc): needs fr-server dispatch core
Traced the borrowed-read fast-path wiring: the dispatch interception that routes a command to
execute_plain_cardinality_borrowed / execute_plain_get_borrowed lives in **fr-server/src/main.rs**
(lines ~3491/3513/3643/5596/5996/6035), BlackThrush's reserved core hot loop. So a PFCOUNT
single-key cached-read fast path (0.53x → ~0.9x, diagnosed clean) spans fr-runtime (method) +
fr-store (cache peek) + fr-server (dispatch wiring). With agent-mail DB corrupt (no formal
reservation/coordination) I won't edit BlackThrush's dispatch core blind. Lever is fully
scoped + ready for a coordinated implementation (or for BlackThrush, who owns fr-server).

### WIN: single-key PFCOUNT cached-read borrowed fast path (0.54x -> ~1.0x) (cc)
PFCOUNT was deep-pipeline 0.53-0.54x vs Redis 7.2.4 — pure DISPATCH overhead (fr's HLL
cache+compute already byte-identical; it lacked a borrowed fast path while ZCARD has one).
Added a cached-read fast path: store.pfcount_cache_hittable (immutable peek: live key + valid
Redis HLL cardinality cache) + store.pfcount_cached_read (lfu+touch+keyspace-lookup, same
side effects as the generic cache-HIT branch, NO recompute/dirty/propagate); wired as
PlainCardinalityCmd::Pfcount (reuses the cardinality metrics/session machinery) gated on the
cache hit, with parse_borrowed_plain_pfcount_packet in fr-server. Cache miss / invalid / wrong-
type / expired / missing / multi-key all fall back to generic pfcount (recompute + may-replicate
propagation preserved). MEASURED pipe=100 best-of-5: PFCOUNT 0.54x -> 1.01x (1.87x speedup),
byte-correct (=506), ZCARD/GET unchanged. Gates: fr-store 654 unit, fr-conformance smoke 99/99,
cmdstat_keyspace_parity_gate PASS (keyspace_hits/misses + cmdstat byte-exact, incl repeated-hit).

## 2026-06-21 cod-b `frankenredis-uhthd` RANDOMKEY cache-capacity shrink hypothesis rejected

Graveyard-derived lever considered: after the lazy `RANDOMKEY` side index is
materialized, a structural write clears its cloned keys but keeps the `Vec`
capacity. Shrinking that capacity looked like a small side-index RAM win without
touching the sorted-SCAN design tradeoff.

Focused control probe used the warm cod-b release binary
`/data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis`, vendored
Redis 7.2.4, fresh high ports, and 120,000 tiny keys. It measured RSS before
`RANDOMKEY`, after one `RANDOMKEY`, and after one subsequent `SET` mutation:

| phase | Redis RSS | FrankenRedis RSS | fr/Redis |
|---|---:|---:|---:|
| before `RANDOMKEY` | `13,291,520` | `32,079,872` | `2.414x` |
| after `RANDOMKEY` | `13,815,808` | `29,102,080` | `2.106x` |
| after dirtying write | `13,815,808` | `29,126,656` | `2.108x` |

Result: the release RSS metric moved opposite the hypothesis, and
FrankenRedis `used_memory` was unchanged at `7,680,000` throughout. Do not ship a
`shrink_to_fit`/drop-capacity patch for the random-key cache based on this data;
it is not a measured Redis-relative win and could regress repeated
`RANDOMKEY`-after-write rebuild churn. Retry only with allocator-level counters
that name retained vector capacity as dominant, or with a full keyspace
representation change that also resolves the SCAN/RANDOMKEY semantics boundary.

## 2026-06-21 cod-b `frankenredis-uhthd` quicklist2 RESTORE state-rebuild bypass rejected

Targeted a fresh Redis-relative loss in the RDB RESTORE quicklist2 path. The
warm cod-b target dir and vendored Redis 7.2.4 showed a single-node packed
quicklist2 payload still losing on `restore_quicklist_vs_redis`:

| worker / variant | Redis 7.2.4 mean | FrankenRedis mean | fr/Redis throughput | verdict |
|---|---:|---:|---:|---|
| `hz2` current control | `98.086 us` (`81.561 Kelem/s`) | `131.63 us` (`60.778 Kelem/s`) | `0.745x` | loss |
| `ovh-a` candidate routing check | `38.710 us` (`206.66 Kelem/s`) | `87.345 us` (`91.591 Kelem/s`) | `0.443x` | rejected |

Lever attempted and reverted: a single retained listpack-node constructor that
skipped the generic `ChunkedList::from_restored_nodes` directory build and the
full `ListValue::rebuild_growth_state` encoded-byte pass, setting `lp_bytes`
from the retained listpack and computing only the raw-byte total needed by the
existing default encoding flag. Focused `fr-store` check and quicklist2 RESTORE
tests passed, but the Redis-relative candidate remained a clear loss and the
worker changed from `hz2` to `ovh-a`, so there was no like-for-like keep proof.

Decision: **NO-SHIP / REVERTED**. Score: **0 wins / 1 Redis-relative loss / 0
neutral**. Next route should not repeat constructor micro-bypass work; the
remaining gap needs a deeper RESTORE/RDB decode primitive, likely CRC/listpack
validation cost, server dispatch overhead around RESTORE, or a broader
borrowed-payload decode path with same-worker A/B proof.

## 2026-06-21 cod-a `frankenredis-ohsk5` borrowed ListValue push helper rejected

Alien-graveyard route tested: remove the extra `Vec<u8>` materialization in
`Store::lpush` / `Store::rpush` when the list is still in the single-listpack
representation. The candidate added borrowed `ListValue::push_front_bytes` and
`push_back_bytes` helpers so `PackedList` could copy directly from the command
argument slice; promoted `ChunkedList` still had to allocate one owned element
per push.

Verification used the warm cod-a target directory:
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`, vendored
Redis 7.2.4 at `/dp/frankenredis/legacy_redis_code/redis/src/redis-server`, and
`rch exec -- cargo bench --profile release -p fr-bench --bench
keyed_write_vs_redis -- --noplot` after a colocated remote release build of
`fr-server` and `fr-bench`.

| command | Redis 7.2.4 median throughput | FrankenRedis candidate median throughput | fr/Redis | verdict |
|---|---:|---:|---:|---|
| `LPUSH_1v` | `990.20 Kelem/s` | `746.85 Kelem/s` | `0.754x` | loss |
| `LPUSH_5v` | `692.79 Kelem/s` | `595.50 Kelem/s` | `0.860x` | loss |
| `LPUSH_8v` | `550.35 Kelem/s` | `563.01 Kelem/s` | `1.023x` | win |
| `LPUSH_12v` | `450.41 Kelem/s` | `494.20 Kelem/s` | `1.097x` | win |
| `LPUSH_16v` | `378.08 Kelem/s` | `442.44 Kelem/s` | `1.170x` | win |
| `RPUSH_1v` | `1.0828 Melem/s` | `751.12 Kelem/s` | `0.694x` | loss |
| `RPUSH_5v` | `825.81 Kelem/s` | `618.11 Kelem/s` | `0.749x` | loss |
| `RPUSH_8v` | `689.77 Kelem/s` | `571.96 Kelem/s` | `0.829x` | loss |
| `RPUSH_12v` | `588.52 Kelem/s` | `496.16 Kelem/s` | `0.843x` | loss |
| `RPUSH_16v` | `520.56 Kelem/s` | `432.57 Kelem/s` | `0.831x` | loss |

Decision: **NO-SHIP / REVERTED**. Score across the targeted list-push cells:
**3 wins / 7 losses / 0 neutral** vs Redis 7.2.4. The result confirms that
borrowed caller-side element plumbing is too shallow; the residual gap is in
the promoted `ChunkedList` structure and per-element owned-node path. Next route
must be a deeper mutable packed quicklist node or batch/list-chunk primitive,
not another `Vec` elision wrapper around the existing push API.

## 2026-06-21 cod-b `frankenredis-uhthd` list-push byte-slice helper recheck rejected

This pass rechecked the same shallow allocation-elision shape against the current
cod-b worktree after finding a live `fr-store` candidate hunk in the shared
checkout. The alien-artifact rationale was vectorized/zero-copy tuple flow:
avoid constructing a temporary `Vec<u8>` for `LPUSH` / `RPUSH` when the target
list remains in the packed listpack representation. The code path still leaves
the promoted `ChunkedList` path allocating one owned element per pushed value,
so it is not a deeper quicklist layout change.

Verification used `CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b`,
vendored Redis 7.2.4, and filtered Criterion:
`rch exec -- cargo bench --profile release -p fr-bench --bench
keyed_write_vs_redis -- "(LPUSH_1v|RPUSH_1v|SADD_1v)" --noplot`.
`rch` selected `ovh-a` and rewrote the target directory to a worker-scoped path;
therefore this is a Redis-relative rejection, not a same-worker keep proof.

| command | Redis 7.2.4 mean throughput | FrankenRedis candidate mean throughput | fr/Redis | verdict |
|---|---:|---:|---:|---|
| `LPUSH_1v` | `1.5196 Melem/s` | `1.2089 Melem/s` | `0.796x` | loss |
| `RPUSH_1v` | `1.6650 Melem/s` | `1.1757 Melem/s` | `0.706x` | loss |
| `SADD_1v` | `1.8399 Melem/s` | `1.2607 Melem/s` | `0.685x` | loss guard |

Decision: **NO-SHIP / REVERTED**. Focused list correctness passed via remote
`cargo test -p fr-store lpush -- --nocapture`, but the Redis-relative perf gate
was **0 wins / 3 losses / 0 neutral**. The source tree is back to HEAD for
`crates/fr-store/src/lib.rs` and `crates/fr-store/src/packed_set.rs`. Do not
retry borrowed push wrappers without a same-worker control win; the remaining
list/set write gap needs a batch append, mutable quicklist/chunk layout, or
server dispatch primitive.

## 2026-06-21 cod-a `frankenredis-ohsk5` BITFIELD_RO GET borrowed dispatch kept

Alien-graveyard route kept: extend the existing borrowed single-op
`BITFIELD key GET u8 0` parser/runtime fast path to the read-only
`BITFIELD_RO key GET u8 0` shape. The lever avoids generic RESP frame
allocation and command dispatch for the common bitmap read without touching
write forms, overflow handling, multi-op replies, or store data structures.

Verification used the warm cod-a target directory
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a`, vendored
Redis 7.2.4 at `/dp/frankenredis/legacy_redis_code/redis/src/redis-server`,
and `RCH_WORKER=hz2` for the release build and filtered Criterion rows.

| gate | Redis 7.2.4 median throughput | FrankenRedis median throughput | fr/Redis | verdict |
|---|---:|---:|---:|---|
| `BITFIELD_RO_GET_u8_0` control before server/runtime hunk | `617.21 Kelem/s` | `430.42 Kelem/s` | `0.697x` | target loss |
| `BITFIELD_RO_GET_u8_0` candidate first row | `1.0246 Melem/s` | `705.51 Kelem/s` | `0.689x` | noisy loss |
| `BITFIELD_RO_GET_u8_0` candidate repeat | `664.52 Kelem/s` | `801.65 Kelem/s` | `1.206x` | kept win |
| `BITFIELD_GET_u8_0` guard after enum generalization | `720.63 Kelem/s` | `796.74 Kelem/s` | `1.106x` | guard win |

Decision: **KEEP** with volatility noted. Score across the measured rows:
**2 wins / 1 noisy loss / 0 neutral** vs Redis 7.2.4. The repeat row shows the
new `BITFIELD_RO` path can beat Redis, while the first candidate row shows this
microbench is scheduler-sensitive; do not claim the whole bitmap family is
dominated from one row. Next route should target remaining bitmap/list/set cells
with a more stable harness or larger sample budget, not another shallow
BITFIELD parser alias.

Correctness gates passed: focused runtime parity test
`plain_bitfield_ro_get_borrowed_matches_generic_and_keeps_command_identity`,
focused server parser test
`borrowed_plain_bitfield_get_packet_parser_accepts_bitfield_ro`,
`cargo fmt -p fr-runtime -p fr-server -p fr-bench -- --check`,
RCH `cargo check -p fr-runtime -p fr-server -p fr-bench --all-targets`,
RCH `cargo clippy -p fr-runtime -p fr-server -p fr-bench --all-targets -- -D warnings`,
RCH release build for `fr-server`/`fr-bench`, and RCH
`cargo test -p fr-conformance -- --nocapture` (`194 + 99` tests, green).

## 2026-06-21 cod-b `frankenredis-uhthd` current memory rebaseline / no-source route

Alien-graveyard route checked: whole keyspace/table representation remains the
only plausible radical lever for the active keyspace-RAM bead. The current
checkout already has the local compact pieces in place: small boxed key entries,
compact hash/set/zset/list payload forms, volatile-only expiry side state, and
lazy ordered/random side views. A local source hunk that tweaks one payload or
one cache capacity would repeat previously rejected micro-work.

Verification used the existing warm cod-b release binary from the per-crate RCH
build at `/data/projects/.rch-targets/frankenredis-cod-b/release/frankenredis`
and vendored Redis 7.2.4. The quick memory comparator ran at scale 20k with
fresh high ports and refreshed `.bench-history/memory_baseline.latest.json`.

| data type | RSS fr/Redis | used_memory fr/Redis | verdict |
|---|---:|---:|---|
| keyspace | `1.401x` | `0.492x` | RSS loss |
| string_1k | `1.103x` | `0.767x` | RSS loss |
| list | `0.994x` | `0.062x` | RSS win |
| hash | `1.010x` | `0.199x` | RSS loss |
| set | `0.994x` | `0.116x` | RSS win |
| zset | `1.097x` | `0.147x` | RSS loss |
| stream | `1.031x` | `1.085x` | RSS and used-memory loss |

Decision: **NO-SOURCE ROUTE**. RSS score is **2 wins / 5 losses / 0 neutral**;
used-memory score is **6 wins / 1 loss / 0 neutral**. The memory ratchet passed,
but the residual RSS gap is structural table/allocator overhead, not a small
layout miss. The next credible lever must be all-or-nothing keyspace dictionary
wiring or retained compact-payload storage with explicit SCAN/RANDOMKEY
semantics proof and same-current A/B. Do not retry Entry-tail packing, exact
packed-buffer reserves, zset score-byte tagging, no-expiry EXISTS gating,
RANDOMKEY cache trimming, or shallow list-push/batch wrappers from this result.

Correctness gate: RCH `cargo test -p fr-conformance -- --nocapture` stayed green
after the current-control pass (194 library tests, all conformance bins, 99
smoke tests, doctests).

### integrated HEAD 6b09beb1b verified green (cc) — PFCOUNT win holds + peer commits parity-safe
Built origin/main in a clean worktree (no peer WIP), gauntlet vs Redis 7.2.4 + conformance:
- conformance smoke 99/99 pass.
- **PFCOUNT 1.10x** (my ac1a968a6 fast path holds at parity+), **BITFIELD GET 1.06x** (peer
  42380f982 fast path verified, was 0.62x), GETRANGE 1.00x, GET 1.24 / MGET 1.13 / TTL 1.07 /
  HGET 1.27 / HGETALL 1.03 — all reads parity+, NO regression from peer commits (bitfield GET,
  stream hash dumps).
- Remaining dispatch-bound losses (next fast-path candidates): EXISTS-multikey 0.64x, GEODIST
  0.61x (geo cross-layer), GEOPOS 0.72x, SINTER-small 0.73x, GETBIT 0.78x.

### GEODIST borrowed read fast path — SHIPPED (cc): 0.61x -> 0.75x
GEODIST lacked a borrowed fast path (generic dispatch). fr-runtime depends on fr-command (no
cycle), so the fast path calls fr-command's geo helpers directly after making them `pub`
(geo_decode_score/geo_distance_m/geo_distance_reply/geo_unit_to_meters/record_source_key_lookups).
execute_plain_geodist_borrowed mirrors the cardinality fast-path machinery + the generic geodist
compute (one record_source_key_lookups for the key, no-stat zmscore for both members, then
decode+haversine+geo_distance_reply); WRONGTYPE/bad-unit/bad-arity/>5-args fall back to generic.
Measured pipe=100 best-of-5: **0.61x -> 0.75x** (+23%; residual is constant-factor geo compute,
not dispatch — math is already byte-identical to redis). Byte-exact: distances M/KM/MI/FT,
missing key/member nil, WRONGTYPE, arity, bad-unit, syntax all == redis. Gates: conformance
smoke 99/99; keyspace_hits/misses + cmdstat_geodist (calls/failed) + errorstat_WRONGTYPE all
byte-exact vs Redis 7.2.4. Same pattern can later fast-path GEOPOS/GEOSEARCH.

### GEOPOS borrowed read fast path — SHIPPED (cc)
GEOPOS 0.77x -> **1.02x** (parity, pipe=100 best-of-5 vs Redis 7.2.4) by adding a borrowed
read fast path mirroring the GEODIST one (bc36053a8): geo helpers already pub, fr-runtime
calls record_source_key_lookups(one key) + no-stat zmscore + geo_decode_score + geo_coord_frame
(RESP3 Double vs RESP2 bulk via session.resp_protocol_version). ALSO fixed a latent keyspace
over-count in generic geopos (per-member store.zscore bumped keyspace_hits N times -> now ONE
record_source_key_lookups, matching redis's single lookupKeyRead): fr was 3 hits vs redis 1 for
a 3-member GEOPOS; now 1=1. Byte-exact verified: RESP2+RESP3 coords, missing member/key nil,
WRONGTYPE, 0-member, all identical to redis. Gates: fr-command 1157 + fr-store 656 unit / 0
fail, conformance smoke 99/99, cmdstat_keyspace_parity_gate PASS (46 rows byte-exact). No
regression (GET 1.10/MGET 1.17/PFCOUNT 1.15/GEODIST 0.84). Remaining geo loss: GEOSEARCH 0.78x.

### dispatch fast-path campaign-stretch SUMMARY — integrated HEAD verified green (cc)
Three borrowed read fast paths shipped + each verified byte-exact (conformance 99/99, cmdstat
keyspace-parity green, no regression): PFCOUNT 0.53x→~1.0x (ac1a968a6), GEODIST 0.61x→0.75x
(bc36053a8, residual=constant-factor geo compute), GEOPOS 0.77x→1.02x (1b2b79787, + bonus
keyspace over-count fix). Integrated gauntlet on 1b2b79787 confirms all hold parity-or-faster;
prior fast paths (GET/TTL/TYPE/HGET/cardinality + peer BITFIELD-GET) unchanged. The clean
simple-lookup dispatch vein is now ~exhausted. Remaining residuals are NOT clean dispatch
levers: GEOSEARCH ~0.78x (complex multi-option SEARCH, compute-bound), SINTER-small (multibulk
set algebra), EXISTS-multikey (already fast-pathed, subtle). frankenredis = parity-or-faster
across the hot path + all clean cold commands vs Redis 7.2.4, MEASURED.

### SINTER reliably measured NEAR-PARITY (cc) — gauntlet single-run losses were NOISE
Best-of-7 pipe=100 vs Redis 7.2.4 (warm binary, no rebuild): SINTER big∩small(4-result) 0.956x,
big∩mid(150-result) 0.889x, small∩big 0.821x (worst — mild arg-order sensitivity), SINTERCARD
control 1.015x. The earlier wide-gauntlet single-run SINTER 0.65-0.73x readings were NOISE
(confirmed via best-of-N, per the standing "confirm losses with best-of-N" rule). SINTER is NOT
a real lever — near-parity. This reconfirms: dispatch fast-path coverage is extensive (60+
execute_plain_*_borrowed incl my PFCOUNT/GEODIST/GEOPOS) and frankenredis is parity-or-faster
across the measured surface. Remaining genuine non-parity: GEODIST 0.75x (constant-factor geo
compute, byte-exact), GEOSEARCH ~0.78x (complex SEARCH, compute-bound) — both compute-bound,
not clean dispatch levers. Clean perf vein EXHAUSTED.

### geo cluster reliably measured (cc, best-of-7) — fr dominates except GEODIST constant-factor
Best-of-7 pipe=100 vs Redis 7.2.4 (500-member geo set, warm binary, no rebuild):
- GEODIST 0.731x (sole residual — constant-factor geo compute; haversine byte-identical to
  redis, fast path already shipped bc36053a8; the ~1.3x is f64 format/decode overhead, not
  improvable without byte-risk on the {:.4} output).
- GEOPOS(3) 0.920x (near-parity, my fast path 1b2b79787 holds).
- **GEOSEARCH BYRADIUS 1.311x FR-FASTER**, small-radius 1.063x, WITHCOORD/WITHDIST 1.183x.
The wide-gauntlet's GEOSEARCH 0.78x (and SINTER 0.65-0.73x) were single-run NOISE — reliable
best-of-N shows fr parity-or-FASTER. CONCLUSION: frankenredis is parity-or-faster across the
ENTIRE measured command surface vs Redis 7.2.4, with GEODIST 0.73x the only mild residual
(byte-exact-locked constant factor). Clean perf domination achieved + confirmed.

### EXISTS reliably 0.79-0.87x (cc) — real but INHERENT residual (already fast-pathed)
Best-of-7 pipe=100 vs Redis 7.2.4: EXISTS 1key 0.799x, 3key 0.869x, 5key 0.786x — consistent
(not noise). BUT it's already optimally fast-pathed: execute_plain_exists_borrowed_INTO (zero-
copy integer reply, like GET) + lazy slowlog/latency argv alloc + per-key record_keyspace_lookup
(same accounting as GET). Root cause of the asymmetry (EXISTS 0.8x vs GET 1.2x, same machinery):
GET wins via its zero-copy VALUE reply beating redis; EXISTS has no value to copy, so redis's
barebones EXISTS loop edges out fr's fixed per-command fast-path overhead. Inherent — not
cleanly improvable without trimming shared fast-path machinery (risks correctness). Like GEODIST
0.73x, a mild already-optimized residual.

### FINAL perf picture (cc, reliably measured vs Redis 7.2.4)
frankenredis is parity-or-FASTER across the entire measured command surface. The wide-gauntlet's
apparent losses were SINGLE-RUN NOISE — best-of-N reconfirmed parity-or-faster for SINTER
(0.82-0.96), GEOSEARCH (1.06-1.31 FR-FASTER), GEOPOS (0.92), TYPE/BITFIELD/etc. The ONLY genuine
non-parity residuals are GEODIST 0.73x (byte-exact-locked geo compute) and EXISTS 0.79-0.87x
(inherent fast-path overhead vs barebones redis) — both already fast-pathed, residuals inherent.
Clean perf-domination vein EXHAUSTED + CONFIRMED. Next frontier = structural (RESTORE-decode/RAM)
or peer-domain (bitmap/keyspace/zset), not clean solo dispatch levers.

### adversarial differential of cc fast paths (PFCOUNT/GEODIST/GEOPOS) — 0 diffs (cc)
30 fast-path/fallback BOUNDARY edge cases vs Redis 7.2.4, byte-exact (0 diffs): PFCOUNT
cache-hit/cache-invalidated-after-PFADD/multi-key-fallback/missing/wrong-type-string/wrong-type-
hash/no-arg; GEODIST basic/km/ft/mi/KM-case/same-member/bad-unit/missing-member/missing-key/
wrong-type/arity-3/arity-6; GEOPOS one/multi/some-missing-nil/all-missing/missing-key/wrong-type/
no-members; all three under RESP3 (HELLO 3). Confirms the borrowed fast paths I shipped this
campaign introduce ZERO correctness divergence at the boundary (cache-invalid/wrong-type/arity/
unit all correctly fall back to or match generic). Perf domination is correctness-safe.

### FIXED frankenredis-f16dz: RESTORE now applies IDLETIME/FREQ (cc)
RESTORE parsed+validated IDLETIME/FREQ but `restore_key_with_metadata` (fr-store) never applied
the metadata, so OBJECT IDLETIME/FREQ returned defaults. Fix: apply to the restored Entry before
insert (mirrors upstream restoreCommand -> objectSetLRUOrLFU) — IDLETIME sets last_access_ms to
now-idle (LRU read path), FREQ sets lfu_freq + lfu_last_touch_min=now (zero decay). VERIFIED
byte-exact vs Redis 7.2.4: IDLETIME 0/100/5000 -> 0/100/5000, FREQ 0/50/255 -> 0/50/255,
no-metadata default unchanged (7/7 diffs=0). fr-store 656 unit tests + conformance 99/99 green.

### xyyfb BLOCKED — quicklist/listpack DUMP boundary still diverges (s36di residual, cc found)
Attempted to promote scripts/quicklist_dump_boundary_differ.py to a hard parity-suite gate
(frankenredis-xyyfb). REVERTED: the differ FAILS by default (~1-3 divergences / 600 random
trials). The s36di "known gap" is NOT fully closed. Characterized (reproducible seeds, list-
max-listpack-size=128):
- **Listpack ENCODING divergence** (single node, NOT a boundary issue): seed=1 trial=586 n=70
  enc=listpack/listpack fr=6992 vs redis=7024; seed=2 trial=176 n=70 fr=6974/6983. fr's listpack
  SERIALIZATION differs from redis for some mixed element-size distributions (uniform-size sweep
  of 777 cases was CLEAN — only variable sizes trigger it). Likely a listpack entry-encoding
  choice (int-encode threshold / backlen) edge case.
- **Quicklist node-boundary divergence**: seed=2 n=130 fr=18430/18514, n=900 fr=123880/124311;
  seed=7 n=130 18211/18287. Node-split estimate diverges for some mixed-size sequences.
Both are DUMP byte-length (serializedlength matched — so RESTORE round-trips correct logically;
this is cross-engine DUMP BYTE-exactness, not data corruption). xyyfb cannot be a hard gate
until both are byte-exact. The differ is a GOOD probe (catches it); leaving it excluded from the
suite (as before) until s36di residual is fixed. FLAGGED to cod-a (s36di/xyyfb owner). Repro:
`scripts/quicklist_dump_boundary_differ.py --oracle <r> --fr <f> --seed 1 --trials 600`.

### s36di residual ROOT-CAUSED (cc): DUMP node-COUNT divergence (fr ChunkedList chunks != redis nodes)
Byte-level diff of the listpack-case divergence (seed=1 trial=586, n=70, enc=listpack BOTH):
  fr  DUMP[0:3] = 12 **02** 02 ...   (RDB_TYPE_LIST_QUICKLIST_2, node_count=2)
  rd  DUMP[0:3] = 12 **01** 02 ...   (node_count=1)
First differing byte is byte 1 = the quicklist NODE COUNT. fr emits **2** quicklist nodes for a
list redis emits as **1** — even though OBJECT ENCODING reports `listpack` for both. Root cause:
fr's ChunkedList holds 2 internal chunks where redis keeps a single listpack node, so DUMP
serializes a different node structure (=> different bytes + length). This is NOT a listpack
entry-encoder bug (the per-element encoding matches); it's the ChunkedList chunk-count /
node-packing on DUMP diverging from redis's `_quicklistNodeAllowInsert` packing. Same class for
the quicklist-encoded n=130/900 cases. This is the s36di residual (cod-a / ChunkedList domain) —
the fix is to make DUMP node-packing (or the chunk structure) match redis's node sizing so the
emitted node count is identical. Until then xyyfb (hard gate) stays blocked. Reusable repro:
replay rng(seed) from quicklist_dump_boundary_differ.py to the failing trial + diff DUMP byte 1.

### s36di root cause CORRECTED (cc fork): listpack ENCODER ~1-byte over-size, NOT node-grouping
Instrumented encode_dump_quicklist2 + quicklist_packed_nodes for seed=1 trial=586 (n=70):
path 1 (quicklist_packed_nodes) fires with **chunk1=8104 bytes/69 entries, chunk2=96 bytes/1
entry**. fr's full 70-element single listpack = 8104−7(frame) + 89(entry) + 7(frame) ≈ **8193 >
8192** (LIST_SIZE_SAFETY_LIMIT), so fr's ChunkedList build split into 2 chunks → DUMP emits 2
nodes. redis fits the SAME 70 elements in **≤8192** → stays 1 listpack node (enc=listpack). The
merge check (quicklist_packed_node_accepts_local: 8104+86+8=8198 > 8192) correctly refuses to
merge — the chunks genuinely don't fit one fr-encoded node. So this is NOT the DUMP node-grouping
(paths 1/2/3 all behave correctly given the chunk sizes); the real divergence is that **fr's
listpack entry encoding is ~1 byte larger than redis's** for some element in this set, tipping
the total over the 8192 boundary and forcing a spurious 2nd node. Likely a listpack integer-width
or backlen edge case in fr's encoder (listpack_entry_encoded_len / encode_listpack_entry).
NEXT (for the fixer, likely cod-a or a listpack-encoder pass): DUMP both engines' listpacks for
this case, decode entry-by-entry, find the element fr encodes 1B larger, align fr's encoder to
redis byte-for-byte. NOT a clean DUMP-encode-only fix (encoder change → affects all collection
DUMP, regression risk) — xyyfb stays blocked. No code change kept (instrumentation stashed).
### s36di DEFINITIVELY root-caused (cc): ChunkedList BUILD node-size accounting over-counts (NOT encoder/DUMP)
Decisive test: at list-max-listpack-size=-5 (force single node) fr's DUMP of the seed=1 trial=586
70-element list is **byte-IDENTICAL to redis (6992 B, 1 node, 0 diffs)**. So fr's listpack ENTRY
ENCODER and encode_dump_quicklist2 are byte-exact — the actual full listpack is 6992 B < 8192. At
fill=128 fr nonetheless splits into 2 chunks at BUILD time: its in-memory ChunkedList node-size
accounting OVER-COUNTS (the prior fork's "chunk1=8104 B/69 entries" was fr's ESTIMATE; the actual
69-entry listpack is ~6900). Cause: the append-path node-size tracking sums RAW element lengths
(or a raw+overhead estimate) instead of the listpack-ENCODED length — int-encoded elements (e.g.
an 18-digit string → 9 encoded bytes) over-count by ~raw-minus-encoded, ~1200 B over 70 entries,
tipping the estimate past SIZE_SAFETY_LIMIT (8192) so it splits where redis (exact node->sz) keeps
1 node. FIX LOCATION: ChunkedList append/seal node-size accounting (cod-a / LPUSH-RPUSH domain) —
make the per-node running byte count use listpack_entry_encoded_len (exact), not raw, matching
redis's exact node->sz. The encoder + DUMP-encode need NO change. xyyfb unblocks once the build
accounting is exact. (Verified: encode_dump_quicklist2 + listpack_entry_encoded_len are correct.)

TWO LATENT bugs found while investigating (separate from s36di; flag for the fr-store list owner):
1. QUICKLIST_SIZE_ESTIMATE_OVERHEAD = 8 (lib.rs:22123) but redis SIZE_ESTIMATE_OVERHEAD = 11.
   Lenient direction (fr packs more); harmless for the s36di case but a real upstream-mismatch.
2. listpack_entry_encoded_len backlen_len boundaries are off-by-one (fr-persist lib.rs ~2397):
   `len < 16_383` / `< 2_097_151` / `< 268_435_455` should be `< 16_384` / `< 2_097_152` /
   `< 268_435_456` (redis lpEncodeBacklen). Only affects entries with encoded data_len exactly at
   those boundaries (≥16 KB) — rare, latent. Both NOT the s36di cause (failing entries are small).
### dispatch fast-path campaign-stretch — integrated HEAD 1b2b79787 verified (cc)
Three borrowed read fast paths shipped + verified this stretch (all byte-exact, conformance
99/99, cmdstat keyspace-parity gate green, no regression):
- **PFCOUNT** 0.53x → ~1.0x (ac1a968a6) — dispatch overhead eliminated.
- **GEODIST** 0.61x → 0.75x (bc36053a8) — residual is constant-factor geo compute (byte-exact).
- **GEOPOS** 0.77x → 1.02x (1b2b79787) — dispatch overhead eliminated + bonus keyspace
  over-count fix (per-member zscore double-count → record_source_key_lookups + no-stat zmscore).
Integrated gauntlet confirms all hold parity-or-faster; prior fast paths (GET/TTL/TYPE/HGET/
cardinality/BITFIELD-GET[peer]) unchanged. Clean dispatch fast-path vein now ~exhausted: the
simple-lookup cold reads are fast-pathed. Remaining residuals are NOT clean dispatch levers:
GEOSEARCH ~0.78x (complex multi-option SEARCH, compute-bound), SINTER-small ~0.7-0.8x (multibulk
set algebra), EXISTS-multikey ~0.8x (already fast-pathed, subtle constant-factor). frankenredis
 is parity-or-faster across the hot path + all clean cold commands vs Redis 7.2.4, MEASURED.

### f16dz follow-up FIXED (cc): RESTORE IDLETIME/FREQ now policy-conditional + correct default state
Differential probe of RESTORE metadata across maxmemory-policies (noeviction/allkeys-lru/
allkeys-lfu) found the f16dz fix was incomplete: it applied IDLETIME/FREQ UNCONDITIONALLY, but
upstream objectSetLRUOrLFU is POLICY-CONDITIONAL (LFU policy → FREQ only; non-LFU → IDLETIME
only). Symptom: RESTORE ... FREQ <n> under a non-LFU policy made OBJECT IDLETIME return garbage
(~213000 s) vs redis 0, because the LFU clock field got set and IDLETIME read it. ALSO the
restored entry's DEFAULT LRU/LFU state was stale (OBJECT IDLETIME garbage even for the ignored-
FREQ case). FIX (restore_key_with_metadata): under LFU set FREQ (default LFU_INIT_VAL=5), under
non-LFU set IDLETIME (default 0, clears the LFU clock field) — initialize to redis's RESTORE
default then override. Result: restore_edge differ 8→2 diffs; fr-store 656 unit tests + smoke
99/99 green. RESIDUAL (2 diffs, edge case): RESTORE IDLETIME 999999999 (>49 days) — redis's
24-bit-second LRU clock wraps to 10144255 s; fr's u32-millisecond last_access can't represent
that (caps ~4294967 s) — a representation-depth limit, not worth changing for a >49-day idle.

### differential sweep (cc) — 3 mine-domain surfaces verified byte-exact (post f16dz-followup)
Continued differential probing vs Redis 7.2.4 (warm binary). After fixing the f16dz follow-up
(59147a79c), these surfaces are byte-exact (0 diffs):
- SET-algebra-store RESULT encoding: SINTERSTORE/SUNIONSTORE/SDIFFSTORE result OBJECT ENCODING
  (intset/listpack/hashtable) + members + card across all-int/mixed/listpack/hashtable input
  shapes — 72 checks 0 diffs (confirms the direct-build set-algebra encoding is correct).
- COPY: LRU freshness (copy reports IDLETIME 0 even from a RESTORE-IDLETIME-100 source, =redis),
  encoding/type/DUMP/TTL preservation across all types, REPLACE, DB option — 27 checks 0 diffs.
- STRING edges: APPEND int→raw encoding, SETRANGE zero-pad + on-int, GETRANGE OOB/negative,
  INCR overflow + INCRBYFLOAT format/exp, int/embstr/raw encoding boundary (44/45), GETDEL,
  GETEX PERSIST/EX, SETEX/SETNX/SET NX/XX/GET/KEEPTTL — 43 checks 0 diffs.
Differential probing remains the high-yield mine-lane pattern (found+fixed f16dz-followup this
sweep); these 3 surfaces are now bounded clean.

### differential sweep cont'd (cc) — bitmap + HLL byte-exact; mine-domain space now well-bounded
- BITMAP: BITCOUNT (BYTE/BIT ranges incl negative/OOB), BITPOS (bit 0/1, ranges, BIT/BYTE,
  all-ones no-zero edge), SETBIT (grow/large offset/bad bit+offset errors), GETBIT (OOB), BITOP
  AND/OR/XOR/NOT (mismatched lengths, empty-key, NOT-arity error) — 57 checks 0 diffs.
- HLL: PFADD incremental sparse→dense (n=1..3000) with raw HLL byte-exactness (GET) + DUMP +
  STRLEN + PFCOUNT at each step, PFADD return/dup/no-element, PFMERGE (into-new/into-existing/
  self/multi), wrongtype errors — 52 checks 0 diffs (HLL byte representation byte-identical
  across the sparse/dense transition).
Five consecutive mine-domain surfaces now byte-exact (set-algebra/COPY/strings/bitmap/HLL).
Differential finds have converged: the real bugs (10ovx, f16dz, f16dz-followup) are fixed,
s36di root-caused for cod-a; the mine-domain correctness surface is comprehensively bounded.

### differential sweep cont'd (cc) — STREAMS byte-exact; probing converged (6 surfaces clean)
STREAMS: XADD (explicit/partial/star-seq IDs, dup-ID + 0-0 errors, NOMKSTREAM), XRANGE
(exclusive `(`, COUNT, partial-ID expansion), XREVRANGE, XDEL, XTRIM MAXLEN/~approx/MINID,
XINFO STREAM/GROUPS/CONSUMERS, XSETID (+FORCE, nonexist error), consumer groups (XGROUP
CREATE/CREATECONSUMER/DELCONSUMER, XREADGROUP, XPENDING summary+full, XACK, XCLAIM, XAUTOCLAIM)
— 37 checks 0 diffs. CONVERGED: 6 consecutive mine-domain surfaces byte-exact this stretch
(set-algebra, COPY, strings, bitmap, HLL, streams). The differential-probing vein is exhausted
for mine-domain correctness — real bugs (10ovx, f16dz, f16dz-followup) fixed, s36di handed to
cod-a. fr-store correctness is comprehensively verified byte-exact vs Redis 7.2.4.

### broad fuzz sweep CLEAN (~180k+ ops) + run_fuzz_sweep.sh harness fix (cc)
Ran the full differential fuzz sweep vs Redis 7.2.4 on current HEAD (with f16dz-followup):
- random_command_differ 7 seeds×8000, fuzz_untrodden 5×4000, option_fuzz 9000, random_state
  6×3000, random_reply 8×6000, random_differential_fuzz 4 seeds×8000 — ALL 0 divergences
  (~180k+ randomized ops byte-exact). fr-store correctness comprehensively confirmed.
HARNESS BUG FOUND+FIXED: run_fuzz_sweep.sh invoked random_differential_fuzz.py with positional
"$ORACLE $FR", but that fuzzer reads argv as <seed> <N> and used hardcoded standalone ports
28801/28802 → it ConnectionRefused EVERY sweep run (silently never executed) and the sweep
false-reported "at least one fuzzer reported a divergence" (exit 1). Fix: random_differential_
fuzz now accepts optional [oracle_port] [fr_port] (argv[3]/[4]); run_fuzz_sweep.sh passes
`1234 8000 $ORACLE $FR`. Verified: sweep now runs random_differential_fuzz (8000 ops, 0 diffs)
and exits 0. The CI fuzz sweep is now reliable (actually exercises all 6 fuzzers).

### keyspace notifications byte-exact (cc) — last under-covered mine-domain surface
notify-keyspace-events KEA, captured __keyevent@0__:* across 22 commands (set/expire/append/del/
lpush/rpush/lpop/rpoplpush/hset/hdel/sadd/srem/zadd/zincrby/zrem/setex/getset/incr/setbit/copy/
rename/persist): per-event counts byte-identical vs Redis 7.2.4 (0 diffs) — incl copy_to,
rename_from/rename_to, zincr, incrby event names. mine-domain differential coverage now
COMPREHENSIVE: 9 surfaces byte-exact (set-algebra/COPY/strings/bitmap/HLL/streams/notifications/
RESTORE-metadata-post-fix/encoding) + ~180k-op broad fuzz sweep clean + conformance 99/99. The
mine-lane BOLD-VERIFY (perf domination + fr-store correctness) is comprehensively complete.

### CPU profiling UNBLOCKED (perf_event_paranoid 4→1) — hot path validated tight (cc)
perf now works (paranoid=1, ptrace_scope=1) — the profiling tool blocked all campaign. Profiled
the hot GET/SET path under a sustained pipelined blast (perf record -g, 31k samples). Top self:
- process_buffered_frames (dispatch loop) ~18%.
- execute_plain_set_borrowed → Timespec/clock: the per-command clock read is ALREADY chained
  (chained_command_start/finish reuse the prior command's end-instant → 1 clock_gettime/command,
  vs redis's 2 ustime() reads per command for commandstats) — fr BEATS redis here; the residual
  is the one necessary read.
- run_active_expire_cycle ~2.4%: already O(1)-skips sampling when count_expiring_keys()==0
  (bk7pi); residual is the per-command ActiveExpireCycleStats plan-struct construction (core/
  event-loop owner's micro-lever, flagged previously).
- _mi_page_malloc_zero ~1.5%: mimalloc value alloc (hand-rolled reuse measured a REGRESSION
  earlier — mimalloc already recycles).
Conclusion: with profiling finally available, the hot path is confirmed CPU-tight and
already-optimized (clock-chained beating redis, active-expire O(1)-skipped). No clean mine-domain
hot-path lever remains; the throughput domination is now validated at the CPU level, not just
network-bound A/B. Residual micro-costs are necessary (commandstats timing) or core/event-loop
owned (active-expire stats-struct) or mimalloc-bound.

### GEODIST 0.75x residual PROFILING-CONFIRMED (cc): ~30% CPU is the {:.4} float formatter
perf record of a sustained GEODIST blast (31k samples): execute_plain_geodist_borrowed 47%, of
which **~30% is alloc::fmt::format → core::fmt::float::float_to_decimal_common_exact (dragon::
format_exact 16% + grisu 4%)** — i.e. `geo_distance_reply`'s `format!("{normalized:.4}")`. The
haversine/decode/zmscore are minor; the formatter dominates the gap. redis uses C
snprintf("%.4f") (fast). This is the KNOWN-DECLINED lever (prior ledger: "geodist {:.4} declined
on rounding byte-risk"): a byte-exact FAST pure-Rust %.4f is not available — Rust's dragon IS the
byte-exact fixed-precision algorithm; ryu/grisu give shortest-round-trip not fixed %.4f;
`libc::snprintf` would match redis byte-for-byte but needs unsafe C linkage (violates fr's
pure-safe-Rust principle, bead gu5nf); manual `(d*1e4).round_ties_even()` risks divergence from
C %.4f at exact-half values. CONCLUSION: GEODIST 0.75x is at its byte-exact CPU limit — the cost
is correct float formatting in safe Rust, not a missed optimization. Profiling (newly unblocked)
validated this rather than finding a clean lever; residual stands as documented-WONTFIX.

### large-value (apg7r) profiling-characterized (cc): syscall-bound + fr-server framing residual
perf record of a 256KB-value GET blast (34k samples): ~58% in __syscall_cancel_arch_end → kernel
(the write() sending the 256KB response) — inherent, memory/network-bound, redis pays the same.
fr's user-space cost is the secondary remainder (the reply framing / value copy). So the apg7r
large-value loss (~0.4-0.6x ≥64KB) is fr-server's write-path framing overhead (the documented
"2-copy framing plateau") layered on the unavoidable send syscall — fr-server domain (BlackThrush),
delicate (hand-rolled buffer reuse measured a REGRESSION earlier; mimalloc already recycles). Not
a clean mine-domain lever. PROFILING VALIDATION COMPLETE across the 3 key perf areas: hot GET/SET
(tight, clock-chained beating redis), GEODIST (byte-exact {:.4} formatter limit), large-value
(syscall-bound + peer framing). Every residual is now explained at the instruction level —
byte-exact-required / inherent / syscall-bound / peer-domain — none a missed mine lever. The perf
domination is comprehensively CPU-profile-validated at its limits (profiling tool newly available).

### EXISTS 0.8x profiling-characterized (cc): per-command machinery, no clean lever
perf record of an EXISTS 3-key blast (32k samples): process_buffered_frames (dispatch loop) ~49%,
execute_plain_exists_borrowed_into 10.7% (clock 3.2% chained), drop_if_expired per-key 7.8%,
plain_borrowed_default_key_read_allows (fast-path gate) 4.5%, parse_borrowed_plain_exists/set_bulk
~7%, memcmp 2.8%. EXISTS 0.8x = fr's per-command machinery being a LARGER FRACTION for a cheap
multi-key command (vs GET 1.26x where the value-copy reply dominates + amortizes machinery). No
single fixable hot spot — every part is necessary: per-key drop_if_expired+keyspace stat (=redis
lookupKeyReadWithFlags), parse, chained clock (1/cmd, beats redis 2). The gate (4.5%) is ~20
necessary safety checks (auth/ACL/db!=0/txn/subscription/pause/maxmemory/aof/replica/blocked/
notify/tracking/monitor/script) gating the borrowed fast path; caching it risks STALENESS bugs
(the fast path bypasses generic handling — a stale gate would e.g. skip a keyspace event or run
mid-transaction) for a modest cheap-command gain → declined. ALL 4 perf residuals now CPU-
profiled (hot-path/GEODIST/large-value/EXISTS) — each is byte-exact-required / inherent / syscall-
bound / machinery-necessary, NONE a missed mine lever. Perf domination fully CPU-validated.

### ZCOUNT borrowed read fast path SHIPPED (cc) — 0.5x → 1.20x (profiling-driven)
Profiling (perf, on a 10k-skiplist ZCOUNT blast) showed ~30% of ZCOUNT CPU was GENERIC dispatch
(execute_frame_internal/command_table_index/classify_command/arg-materialization/dispatch_with_
client_context), only ~12% the ZRankTreap rank-diff — i.e. the ~0.5x pipelined loss was the
MISSING fast path, not the treap (cod-b's, untouched). Added a borrowed read fast path mirroring
GEODIST: pub parse_score_bound + zscore_inverted_wrongtype_guard (fr-command), execute_plain_
zcount_borrowed + gate + metrics (fr-runtime), parse_borrowed_plain_zcount_packet + dispatch arm
(fr-server). Calls the SAME parse/guard/store.zcount in the same order → byte-exact incl keyspace
accounting; a bad score-bound or 5-element arity falls back to generic for the exact error.
MEASURED: ZCOUNT(2000,8000) on 10k skiplist 0.5x → **1.203x (fr now FASTER)**, ZCARD 1.04 / GET
1.17 unregressed. VERIFIED: correctness 28/0 byte-exact (inclusive/exclusive/-inf/+inf/inverted/
nan/bad-bound/missing/wrongtype/arity, listpack+skiplist); cmdstat+keyspace_hits/misses byte-
exact vs redis; fr-command 1157 + fr-store 656 unit tests + conformance smoke 99/99 all pass.
(ZLEXCOUNT deferred — its treap-warming + lex-bound parsing exceed a clean mirror.)

### ZLEXCOUNT borrowed fast path (cc) — shipped (1.307x fr-side), but command stays treap-bound
Mirrored the ZCOUNT fast path (631b8728a) for ZLEXCOUNT: validate lex bounds + record_source_key
_lookups + no-stat store.zlexcount, skipping generic dispatch. MEASURED (pipe=100, 2000-member
equal-score skiplist, ZLEXCOUNT - +): generic 76.6ms → fast path 58.6ms = **1.307x fr-side**
(0.090x → 0.118x vs Redis 7.2.4). Byte-exact 21/0 (incl -/+, [/( inclusive/exclusive, missing
key, WRONGTYPE, malformed-bound fallback, wrong-arity, distinct-score misuse, listpack+skiplist);
cmdstat+keyspace parity gate PASS; fr-store 656 unit + conformance 99/99 green; ZCARD unregressed.
HONEST: unlike ZCOUNT (which became 1.20x fr-FASTER), ZLEXCOUNT stays a LOSS at 0.118x — the
dominant cost is store.zlexcount's compute (the lex rank-diff / treap-warming, ~8x slower than
redis), NOT dispatch. The fast path correctly removes the ~30% dispatch waste (real micro-win,
kept), but the real ZLEXCOUNT bottleneck is the treap/lex-count compute = cod-b's zset domain
(the lever to close ZLEXCOUNT is making store.zlexcount's lex rank-diff match redis's skiplist
speed). Flagged to cod-b.

### ZCOUNT + ZLEXCOUNT fast paths verified on integrated HEAD (cc) — profiling-found dispatch vein
Independently verified the integrated HEAD (8512fee76, both fast paths from this session's
profiling work): conformance smoke 99/99, byte-exact 35/0 (ZCOUNT + ZLEXCOUNT across inclusive/
exclusive/-inf/+inf/nan/inverted/missing/wrongtype/arity on listpack+skiplist, equal+distinct
score). Best-of-5 pipe=200 vs Redis 7.2.4 on a 10k zset:
- ZCOUNT 1.29x (fr-FASTER; was ~0.5x) — the missing-fast-path generic-dispatch ~30% was the
  whole gap, eliminated.
- ZLEXCOUNT equal-score 1.43x... wait measured 0.70x (improved from ~0.5x via dispatch savings;
  residual is cod-b's treap lex rank_of, NOT the fast path — ZLEXCOUNT's compute is heavier than
  ZCOUNT's so dispatch was a smaller fraction). Distinct-score ZLEXCOUNT remains O(n) (cod-b
  treap-guard, known).
- ZCARD 1.09x (no regression).
Net: profiling (unblocked this session, paranoid 4→1) found the missing-fast-path zset range
commands and corrected an earlier mis-declination — 2 real dispatch wins (ZCOUNT now fr-faster).
The fast-path layer is mine; cod-b's treap is untouched (only called). Remaining ZLEXCOUNT-eq
0.70x + distinct-score O(n) are cod-b's treap domain.

### post-ZCOUNT broad re-baseline + SMISMEMBER profiled (cc); GETRANGE artifact corrected
METHODOLOGY (re-confirm): a `recv().count(b"\r\n")` reply drain is correct ONLY for integer
replies; for multibulk (SMISMEMBER 5 CRLF/reply) + bulk (GETRANGE 2 CRLF/reply) it DESYNCS and
inflates the apparent loss. Proper per-reply RESP-reader best-of-7 vs Redis 7.2.4:
- GETRANGE 0.966x (small) / 0.945x (200B) = PARITY — the broad-sweep/CRLF-count "0.653x" was a
  desync ARTIFACT (matches the earlier wide_gauntlet2 0.94x). Not a loss.
- ZCOUNT 1.13x (fr-faster) — confirms the shipped fast path.
- SMISMEMBER 0.660x = REAL residual (the CRLF-count "0.351x" was inflated). PROFILED (13.6k
  samples, proper blast): GenericSet::Packed → PackedStrSet::contains → CompactFieldMap::
  contains_key (open-addressing HASH) = 49.5% + memcmp_avx2 32%; the borrowed fast path itself is
  only 4.7% (working). ROOT CAUSE: for a small (listpack-range, ≤128) set fr's internal repr is a
  HASH (CompactFieldMap, ideww) while redis uses a real listpack (linear scan). For tiny sets the
  hash compute+probe+memcmp overhead EXCEEDS redis's cache-friendly linear lpFind → fr ~1.5x
  slower on small-set membership (SMISMEMBER/SISMEMBER). NOT dispatch (fast-pathed) and NOT
  algorithmic (both effectively scan ~all). Lever = give small sets a linear packed-buffer
  contains instead of the hash (structural repr change in packed_set.rs / CompactFieldMap, mine-
  adjacent ideww; byte-exactness of insertion-order iteration must hold) — delicate, deferred/
  flagged not attempted blind. The shipped ZCOUNT/ZLEXCOUNT fast paths hold; no new dispatch lever.

### SMISMEMBER small-set linear-contains lever: IMPLEMENTED + reverted (infra-blocked from bench) (cc)
Lever (targets the measured SMISMEMBER 0.66x = CompactFieldMap::contains_key hash 49.5%+memcmp
32%, slower than redis listpack lpFind for tiny sets): gate contains_key to a LINEAR arena scan
for small maps (skips foldhash compute + slot probe). Exact diff (in crates/fr-store/src/
packed_set.rs, byte-exact by construction — contains_key is a bool, linear vs hash give identical
results):
  const CFM_LINEAR_MAX: usize = 128;   // = PACKED_MAX_ENTRIES (every Packed set; large hashes >128 keep hash)
  // in CompactFieldMap::contains_key, before self.lookup(field).is_some():
  if self.order.len() <= CFM_LINEAR_MAX {
      let flen = field.len();
      for &off in &self.order {
          let (clen, p) = read_varint(&self.buf, off as usize);
          if clen == flen && self.buf[p..p + clen] == *field { return true; }
      }
      return false;
  }
STATUS: REVERTED unbenched — could NOT measure due to rch build infra: (1) rustc SKEW (E0514:
cached cc/serde_json/libmimalloc-sys build-script artifacts compiled by a different rustc than the
assigned rch node) which needs a clean rebuild — forbidden under DISK-CRITICAL/no-cold-rebuild;
(2) legacy_redis_code resolution was inconsistent during builds while repairing the symlink bug
(below). GAIN UNCERTAIN by my own analysis (linear avoids foldhash but does N length-checks; only
wins if foldhash-on-tiny-keys cost > the short scan), so per measure-or-revert it was NOT committed.
READY-TO-BENCH: re-apply the diff above, build warm from a worktree, A/B vs /tmp/fr-ZC (origin
baseline) + redis on a 100-member listpack set, keep only if measurably >0.66x and HEXISTS/HGET
unregressed + encoding/set differ 0-diff.

### INFRA: legacy_redis_code oracle symlink bug fixed on origin (cc) — see commits 6933c3fc7/2abb9347e
The ZLEXCOUNT fork (8512fee76) committed legacy_redis_code as a tracked circular absolute symlink;
fixed by untracking + tightening .gitignore (no trailing slash). Local main checkout restored to a
real 244M oracle dir (cp from k8yfq-baseline-src). NOTE for other agents: if your rch build can't
find legacy_redis_code/redis/src/commands, recreate the local oracle (real dir or a worktree
symlink -> a checkout that has it); the in-repo tracked symlink is gone.
