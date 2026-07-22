## 2026-07-22: SHIPPED — small-length const-size bulk header path in encode_bulk_string_slice; 1.7764x fewer instructions on the per-element encode loop (frankenredis-vlis9)

NEGATIVE-LEDGER-FIRST: reply-encode was closed as "saturated, all single-pass" after bab278487
(fused header KEEP on the borrow path) and the owned-arm propagation REJECT (tight-loop
regression ~1.15%, root-caused: const-size copies the compiler can inline beat a runtime-length
fused-buffer memcpy in per-element loops). This lever is the root-cause-ALIGNED shape neither row
measured: for len<10 / len<100 the `$<len>\r\n` header becomes ONE const-length
`extend_from_slice` (compile-time-size store, no 24-byte stack build) and the per-element
`reserve` + `ilog10` disappear; len>=100 keeps the shipped fused path unchanged. REOPEN EVIDENCE
(fresh live profile, this session): sustained `HGETALL bigh` (10k fields, c50, pinned, release
bin sha256 ad389808120799656da03cf8e46fc7a68a13195e352b7171166ebf1bc606b241) put
`encode_bulk_string_slice` at 26.70% self and `push_len_header` at 22.23% self — the two top
frames of the whole server; kernel only ~5.5%. That satisfies the "material top-ranked self-cost"
bar the 2026-07-10 output REJECT set for reopening output-path work, but the cost is ENCODE, not
flush — writev stays closed.

MEASURED (benches/encode_bulk_small_len.rs, release-perf lto=false, 24 rounds, worker
vmi1149989, bench binary sha256 e1a91804c941a43a878bb90ad9de3bd8c058c095f6670102ad726d80213d2b32,
both arms in one binary, order-balanced AB/BA, HGETALL-shaped per-element loop with bodies):
reference_over_candidate median = 1.776392949 (**1.7764x fewer instructions / 43.71% less**),
effect cv 0.000084%; null (A/A) median 1.000000051, p05..p95 [0.999998570, 1.000001450], null cv
0.000084%. Reference frame self-time median 5.04% (samples 0.77/5.04/13.50 — high-variance
sampling on that worker; the near-zero-variance instruction counts decide, and the correctness
gate proves the code is reached). BIT-IDENTICAL: in-crate `small_len_bulk_header_matches_reference`
(len 0..=300 + 301/511/512/999/1000/1023/1100 × resp3 × Some/None + pinned literals) plus the
bench correctness gate (614 cases); fr-protocol test suite green post-change.

LIVE END-TO-END (same host, pinned, seeded 10k hash, 3000 HGETALL replies at c50, perf stat
instructions:u, 3 reps each): control (sha256 ad389808..., pre-vlis9 HEAD) 12.8618B (cv 0.0003%)
-> candidate (sha256 8120f49257f1d71c64a950ef85cce58257a732221fef21a444d917bce8724dff, HEAD
3013e6110 + vlis9) 8.2357B (cv 0.0007%) = **1.5617x fewer / -35.97% end-to-end** (caveat:
candidate HEAD also contains the concurrent hqca6 runtime keep, measured at ~2.9% by its author —
frame attribution isolates vlis9: push_len_header VANISHED from the candidate live profile, was
22.23% self). Closed-loop rps unchanged (~775/s both arms): the redis-benchmark client parsing
~285MB/s of RESP on 2 cores is the wall-clock bottleneck, so the win is server CPU headroom.
CROSS-ENGINE, same workload: vendored Redis 7.2.4 spends 32.539B instructions:u (cv 0.02%) on the
identical 3000 replies — FrankenRedis post-vlis9 retires **3.95x fewer instructions than Redis**
(2.53x pre-vlis9).

Trigger condition: every RESP2/RESP3 bulk reply element with body length < 100 emitted through
the borrow-encode path — the dominant shape of HGETALL/LRANGE/SMEMBERS/ZRANGE collection replies
and of small GET/HGET bodies. len>=100 and the None/nil arms are byte-for-byte the prior code.
Do NOT extend this to the owned RespFrame arms (that exact propagation is the 2026-07-13 REJECT).
Artifacts: artifacts/optimization/frankenredis-vlis9/20260722T1810Z/.

## 2026-07-22 FoggyOrchid: SURFACE — swarm-brief "pipelined 33-47% writev wall" premise re-verified STALE; 10k-element collection reads now PARITY-OR-FASTER; live cost moved to per-element encode

The 2026-07-22 swarm brief assigned the README's "pipelined throughput 33-47% of redis, lacks
writev/batched reply flush" wall as an architectural lane. Ledger-first: the small-reply P16
writev family is REJECTED (2026-07-10 cod_fr; strace shows exactly 16 cmds/syscall — replies
already perfectly coalesced). Re-measured the one adjacent surface left open by the 2026-07-03
SURFACE (large-output collection reads 0.56-0.63x, "vectored-write lever blocked on agent-mail
down"): fresh release build @HEAD (control sha256
ad389808120799656da03cf8e46fc7a68a13195e352b7171166ebf1bc606b241, /tmp/fr_ctl_3cp5v.bin) vs
vendored redis 7.2.4, both pinned (fr core 2, redis core 3, client 6,7), 10k-element seeds,
interleaved order-balanced 5 trials, medians: PING guard 1.037x | HGETALL(10k) 1.080x |
LRANGE 0 -1 1.088x | SMEMBERS 1.218x | ZRANGE WITHSCORES 1.028x (fr/redis; per-side cv
0.5-7.1%). The 19 days of shipped levers closed the 2026-07-03 gap WITHOUT writev. CONSEQUENCE:
no writev/scatter-gather lever remains on either the small-reply or the large-collection reply
path; do not re-open from the README text (stale). The fresh live profile under sustained
HGETALL(10k) relocates the cost to user-space encode: encode_bulk_string_slice 26.70% +
push_len_header 22.23% + CompactFieldMap::get_index 17.59% (ideww RAM tradeoff, closed) +
__memmove 7.79%; kernel ~5.5%. Encode lever shipped as frankenredis-vlis9 (entry above).
SECONDARY (frankenredis-3cp5v, in_progress): fr-server writer-pool Drained completions DROP the
connection's grown write_buf allocation (inline flush retains it via clear()) — but the offload
path does NOT fire under plain c50 collection reads (writer-thread utime stays 0 ticks through
5k replies); it fires under P16 pipelined 10k-collection reads (utime 3 ticks/thread over 8k
replies). Any 3cp5v A/B must therefore run P16-pipelined large replies and gate on
instructions:u, or it measures dead code. Artifacts:
artifacts/optimization/frankenredis-vlis9/20260722T1810Z/ (profile + fr-vs-redis table).

## 2026-07-22 FoggyOrchid: REJECT — writer-pool Drained completion returning write_buf capacity is UNDECIDABLE below the P16 noise floor; hunk reverted, no source kept (frankenredis-3cp5v)

NEGATIVE-LEDGER-FIRST: no prior row covers returning the drained WriterJob's allocation; the
rejected writer-family rows were queue topology / writer-owned outbox / write_vectored wrappers.
Shipped precedent for the lever family: "replica ACK snapshots retain Vec capacity" (2026-07-13).
Mechanism: flush_writer_job drains the buffer and the Drained arm DROPPED `completion.bytes`
(empty, capacity-retaining) — every offloaded reply forced the next reply to regrow
`conn.write_buf` from zero (Vec-doubling realloc+memcpy chain); the inline-flush path retains
capacity via `clear()`. TRIGGER (measured): offload never fires at plain c50 HGETALL(10k)
(writer-thread utime 0 ticks/5k replies); fires at P16 (12-13 ticks/24k replies) — A/B therefore
runs `redis-benchmark -c50 -P16 -n8000 HGETALL bigh` (10k-field hash) and gates on instructions:u
per fixed work. Post-vlis9 P16 profile ceiling: __memmove 1.13% self, drain_writer_completions
1.38% — expected effect ~1% class.
MEASURED (same host, ctl core 2 / cand core 3 / ctl2-null core 4 [ctl binary again], client
cores 6,7, 6 order-balanced reps of `perf stat -e instructions:u -p <server>` over fixed
`-c50 -P16 -n8000 HGETALL bigh`; control sha256
3497c2605bdbaea9c476755da6c1beadd467267b937f57942768b6e253008bd8, candidate sha256
c334486b152493d3f4ed2a0648589f7db56cdd70a355243edfb7f6e5e9542285 — BOTH arms rebuilt
back-to-back hash-bracketed on the identical tree snapshot (HEAD e40d17234 + peer fr-runtime WIP
fa0a3f97, verified unchanged across both builds; an earlier candidate build was DISCARDED because
the peer WIP hash moved mid-build)): ctl median 38.7318B cv 4.342% | cand median 38.6292B cv
1.805% | null(A/A) ratio median 0.9909 spread [0.8932, 1.0562] | cand/ctl median 0.9952 spread
[0.9179, 1.0236].
VERDICT: REJECT — the candidate median (0.9952) lies INSIDE the A/A null spread; the P16
fixed-work instrument carries an ~8-16% run-to-run spread on this shape (offload-count/epoll-round
variance; contrast P1 fixed-work cv 0.0003%), so the ~1%-ceiling effect is UNDECIDABLE on the only
workload class that triggers the path. Hunk reverted; do not re-land without a decidable
instrument. RETRY PREDICATE: reopen only if (a) a live profile shows write_buf regrow
(RawVec::grow/__memmove under drive_client_output/encode after offload) at >=3% self on a real
workload, or (b) an offload-forcing deterministic harness (fixed offload count per rep, e.g.
throttled-reader socket pair) achieves a null spread tight enough to decide a <=1% effect. The
mechanism itself is real (Drained drops the allocation; inline flush retains it) — it is the
MAGNITUDE that is below the floor.

## 2026-07-22: SHIPPED — single-byte LEB128 fast path in packed_set::read_varint; 5.49% fewer end-to-end instructions on P1 HGETALL(10k) (frankenredis-pipsm)

NEGATIVE-LEDGER-FIRST: vein switch per the graveyard protocol after the writev/batched-flush
blocker (entries above). The ideww closure ("varint decode is a DELIBERATE RAM tradeoff — don't
re-chase per-element compute") covers the FORMAT, not the decode implementation; the fresh
post-vlis9 live profile named `CompactFieldMap::get_index` 14.46% + `HashFieldMapIter::next`
10.93% self as the top user frames at P1 HGETALL(10k) — reopen evidence at the same standard as
vlis9. `read_varint` had no single-byte fast path: every 1-byte length (the universal case for
field/value lens) ran the generic shift-accumulate loop. Change: `read_varint_impl<const FAST>`
returns `(byte, pos+1)` when `byte < 0x80`; multi-byte falls through to the EXACT prior loop;
production routes `::<true>`; arena format untouched (byte-identical, one decoder, 31 call sites
across the packed family). MEASURED: see
artifacts/optimization/frankenredis-pipsm/20260722T2030Z/p1_hgetall_instructions_ab.txt —
cand/ctl instructions:u = 0.94514 (5.49% fewer end-to-end), null/ctl 1.00010 ± 0.00005, effect
~1000x the floor; hash-bracketed builds (ctl e15aaa93.../cand 0cdec1bc..., production-const-only
diff, both arms in both binaries). fr now retires 4.20x fewer instructions than redis 7.2.4 on
identical HGETALL(10k) work. Gates: exhaustive fast==slow varint gate test; fr-store 877+aux
green; fr-store clippy green (required fixing a pre-existing needless_range_loop the current
nightly newly flags in fr-simd:795 — dependency of fr-store, blocked everyone's gate).
FOLLOW-UP CANDIDATES in the same vein (unmined): #[inline] on the still-uninlined
CompactFieldMapIter::next / HashFieldMapIter::next call boundaries (10.88% + 3.61% self frames);
cfm_decode fusion of the two range computations.

## 2026-07-22: SHIPPED — #[inline] on the packed_set per-element call boundaries; 14.32% fewer end-to-end instructions on P1 HGETALL(10k) (frankenredis-citbb)

NEGATIVE-LEDGER-FIRST: no prior row covers inline annotations in fr-store (grep empty). Reopen
evidence: the post-pipsm candidate profile still showed the per-element access chain as SEPARATE
frames (CompactFieldMapIter::next 10.88%, cfm_decode 5.85%, HashFieldMapIter::next 3.61%) — a
non-inlined call boundary paid 20k+ times per HGETALL(10k) reply. Change: #[inline] on
CompactFieldMap::get_index, CompactFieldMapIter::next, HashFieldMapIter::next, cfm_decode —
attributes only, zero semantics. MEASURED (hash-bracketed builds ctl 25e9d780/cand a9084d8c, 5
order-balanced reps): cand/ctl instructions:u = 0.85683 (**14.32% fewer end-to-end**), null/ctl
1.000006 spread +/-0.00003 — effect ~5000x the floor. Inlining fused sink->iter->get_index->
cfm_decode->read_varint into one loop and unlocked cross-function optimization worth ~3x the
varint fast path alone. Frame check: all three frames GONE. fr now 4.90x fewer instructions than
redis 7.2.4 on identical HGETALL(10k) work (12.86B -> 6.64B per 3000 replies across today's three
levers). Gates: fr-store 877+aux green, clippy --all-targets green, benchmark_gate PASS. Details:
artifacts/optimization/frankenredis-citbb/20260722T2125Z/. LESSON (reusable): after any hot-loop
fast-path ships in this crate, re-profile for surviving call-boundary frames — #[inline] on tiny
pub(crate)/iterator fns has repeatedly been left on the table because LLVM won't inline across
codegen units without the hint at lto=false.
