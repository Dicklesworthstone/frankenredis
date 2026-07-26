# Ledger Resurrection Audit — frankenredis

> Meta-Lever #1 of `PERF_CAMPAIGN_2026-07-25`. Owner: cc / STRUCTURAL lane.
> Regenerate with `scripts/ledger_resurrection_audit.py <profile.txt> docs/LEDGER_RESURRECTION.md "<profile description>"`.

Audited ledgers: `docs/NEGATIVE_EVIDENCE.md` (422 `##` + 451 `###` entries) and `docs/perf_negative_evidence_ledger.md` (339 `##` entries). Every heading whose verdict word is REJECT-class — REJECT/REJECTED/NEGATIVE/NO-SHIP/BLOCKER/REVERT/DECLINED/INVALID/UNDECIDABLE — is scored against the void predicates below.

## Method

Both ledgers hard-wrap prose at ~100 columns, so every predicate runs against a whitespace-normalised body. Matching the raw text scores a line-wrapped `Null\nmedian 1.0000000` as *no null control recorded* — the exact provenance error this audit exists to catch. (The first run of this script made precisely that mistake and reported a 95% void rate; the corrected rate is below.)

**Void predicates**, in the order tested:

| Predicate | Test |
|---|---|
| V1 | the claimed ratio lies INSIDE the entry's own null floor. The floor comes from the entry when it records one (null median / null CV); only when the row records no null control at all do we fall back to the era-default floor for the harness shape the row names (`-c1` ±0.1%, in-process instruction counts ±2%, `-c50`/P16 ±10.5%). |
| V2 | no A/A null control recorded at all. |
| V3 / V3n | target-frame self-time recorded as ~0% / not recorded at all. |
| V4 | the decision was gated on `cv < X%` rather than on a null floor. |
| V5 | no binary sha256 recorded. |
| V6 | the row concedes a real, tight, non-noise effect and rejects it anyway on an arbitrary magnitude threshold ("below the 1% keep gate"). §2.3 of the campaign says decidability is set by the null CI, not by a round number. |

**Verdicts.** The campaign's literal predicate list marks nearly every pre-hardening row void, which is true but not actionable, so the result is stratified:

| Verdict | Meaning | Actionable? |
|---|---|---|
| VOID | V1 or V4 fired — the measurement could not have decided the lever. | **Yes — re-run.** |
| GATE-VOID | V6 fired — the measurement *did* decide it and a threshold vetoed it. | **Yes — re-adjudicate.** |
| PROVENANCE | the effect is far outside any plausible floor, but the row is missing null control / sha / self-time. The decision is probably sound; the evidence is not reproducible to the current contract. | No — record only. |
| SOUND | null control + binary sha + self-time all present. | No. |

The **self-time of target frame** column the campaign asks us to rank on does not exist in the pre-hardening rows. Rather than leave it blank, every entry's backticked Rust identifiers are joined against a **fresh symbolized live profile** of the running server, so the column reports what the named frame costs *now* instead of what the row claimed then. A void entry whose target frame is invisible in the live profile is not worth re-running; a void entry sitting on a live frame is.

Profile used for the join: live P16 GET profile — `artifacts/optimization/campaign-20260725-cc/profile-p16-get.txt`, 22,495 samples, fr binary sha256 `4df05cded722cb07fd474e4b74418fec9b9a7646a43afb9ae5032d90a29a61a4` (504 symbols).

## Yield
| Metric | Count |
|---|---|
| REJECT-class entries audited | 195 |
| VOID | 57 |
| GATE-VOID | 1 |
| PROVENANCE | 132 |
| SOUND | 5 |
| Re-run under the corrected harness | see *Re-run results* below |

### Predicate frequency across all audited entries

| Predicate | Meaning | Entries |
|---|---|---|
| V1 | claimed ratio inside the entry's own null floor | 54 |
| V2 | no A/A null control recorded at all | 172 |
| V3 | target-frame self-time recorded as ~0% | 0 |
| V3n | no target-frame self-time recorded | 186 |
| V4 | decision gated on `cv < X%` | 4 |
| V5 | no binary sha256 recorded | 174 |
| V6 | real, tight effect vetoed by an arbitrary magnitude gate | 1 |

## Rehabilitation queue — VOID / GATE-VOID ranked by live self-time

| # | Entry | Ratio claimed | Null floor at the time | Self-time of target frame (live) | Binary sha? | Verdict |
|---|---|---|---|---|---|---|
| 1 | `NEGATIVE_EVIDENCE.md:4166`<br>2026-07-10 cod_fr: REJECT (premise) — current P16 small-reply path is NOT writev-starved; do not retry wrappers over `write_buf` | 1.007 | ±2% — assumed: in-process instr shape | 2.55% (`frankenredis::process_buffered_frames`) | yes | VOID |
| 2 | `NEGATIVE_EVIDENCE.md:4495`<br>2026-07-09 CodexRedisDig: REJECT — GEOHASH multi-member direct wire/streaming encoder — no stable gain vs ORIG | 0.997 | ±2% — assumed: in-process instr shape | 2.55% (`frankenredis::process_buffered_frames`) | no | VOID |
| 3 | `NEGATIVE_EVIDENCE.md:6833`<br>2026-07-04 CrimsonHawk: SURFACE (blocker) — clean single-turn per-crate keyspace-probe/bare-drop vein EXHAUSTED; next levers are multi-part/structural | 1.03 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | 2.55% (`frankenredis::process_buffered_frames`) | no | VOID |
| 4 | `perf_negative_evidence_ledger.md:3765`<br>2026-06-27 AmberRiver: SET drop_if_expired guard — profile-driven, MEASURED ~0-gain, REVERTED | 1.008 | ±10.5% — assumed: -c50 multi-conn shape | 2.55% (`frankenredis::process_buffered_frames`) | no | VOID |
| 5 | `perf_negative_evidence_ledger.md:4031`<br>2026-06-28 AmberRiver: XADD drop_if_expired guard MEASURED ~0-gain (1.015x), REVERTED — gap confirmed structural | 1.01 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | 0.82% (`<hashbrown::map::HashMap<alloc::boxed::Box<[u8]>, fr_store::Entry, foldhash::quality::RandomState>>::get_mut::<[u8]>`) | no | VOID |
| 6 | `perf_negative_evidence_ledger.md:10367`<br>2026-07-14 BlackThrush: NO-SHIP (reverted) — LFU ZRANGEBYLEX member borrow-scan 2->1 is alloc-diluted | 1.101 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | 0.82% (`<hashbrown::map::HashMap<alloc::boxed::Box<[u8]>, fr_store::Entry, foldhash::quality::RandomState>>::get_mut::<[u8]>`) | no | VOID |
| 7 | `perf_negative_evidence_ledger.md:1826`<br>2026-07-14 CalmHeron: NEGATIVE — NO-SHIP. active-expire live-key clone elision is null-drowned (`frankenredis-auer7`) | 1.0368 | ±164% — row: 3x null CV 54.66% | 0.52% (`<fr_runtime::ServerState>::run_active_expire_cycle`) | yes | VOID |
| 8 | `perf_negative_evidence_ledger.md:3691`<br>2026-06-27 AmberRiver land-or-dig: clean-crate lever surface exhausted + agent-mail blocker surfaced | 1.017 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | 0.41% (`<fr_store::Store>::get_string_bytes`) | no | VOID |
| 9 | `NEGATIVE_EVIDENCE.md:19409`<br>2026-07-12 NEGATIVE (validatable frontier saturated across all crates — structural RdbValue is the only real lever) | 1.08 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | 0.29% (`fr_protocol::encode_bulk_string_slice`) | no | VOID |
| 10 | `perf_negative_evidence_ledger.md:46`<br>2026-07-24 CreamPeak: BLOCKER — session-snapshot micro-lever profile is saturated (`frankenredis-6oavn`) | 0.929 | ±10.5% — assumed: P16 multi-conn shape | 0.15% (`<fr_runtime::Runtime>::record_client_session`) | yes | VOID |
| 11 | `NEGATIVE_EVIDENCE.md:4627`<br>2026-07-09 CrimsonHawk: REJECT (0-gain) + SURFACE — SORT is a real EXECUTOR loss (ALPHA 0.58x vs redis) but the reply-clone (into_iter) lever is 0-gai | 1.02 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | 0.10% (`__memcmp_avx2_movbe`) | no | VOID |
| 12 | `perf_negative_evidence_ledger.md:3888`<br>2026-06-27 AmberRiver: list RDB-load `rpush_owned` (avoid redundant clone) — MEASURED ~0-gain, REVERTED | 1.073 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | 0.06% (`mi_free`) | no | VOID |
| 13 | `NEGATIVE_EVIDENCE.md:7`<br>2026-07-25 AzureMouse (cc/STRUCTURAL): REJECT (premise) — the campaign's io_uring submission-batching lever has nothing to amortize; fr is 1.25x FASTE | 1.004 | ±2% — assumed: in-process instr shape | not visible in the live profile | yes | VOID |
| 14 | `NEGATIVE_EVIDENCE.md:177`<br>2026-07-24: BLOCKER — session-snapshot micro profile saturated (`frankenredis-6oavn`) | 0.929 | ±2% — assumed: in-process instr shape | not visible in the live profile | yes | VOID |
| 15 | `NEGATIVE_EVIDENCE.md:2051`<br>2026-07-13: REJECTED (SUB-GATE) — direct-write `write_i64_to_slice` (no tmp+copy) is only +0.51%, below the 1% keep gate | 1.005097 | ±0.001% — row: 2x |null median 1.000000055 - 1| | not visible in the live profile | yes | GATE-VOID |
| 16 | `NEGATIVE_EVIDENCE.md:2728`<br>2026-07-12: NEGATIVE (measured neutral, reverted) — the memset-elision does NOT generalize to HLL dense ENCODE | 0.993 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 17 | `NEGATIVE_EVIDENCE.md:2898`<br>2026-07-12: NEGATIVE (sub-gate, reverted) — zset-listpack score ENCODE per-entry render Vec hoist | 0.953 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 18 | `NEGATIVE_EVIDENCE.md:3257`<br>WHY NO RATIO (blocker, not an omission) | not stated | ±2% — assumed: in-process instr shape | not visible in the live profile | no | VOID |
| 19 | `NEGATIVE_EVIDENCE.md:3979`<br>DECLINED (recorded rejection, not retried): GETSET / GETDEL zero-copy `_into` | 1.077 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 20 | `NEGATIVE_EVIDENCE.md:4737`<br>2026-07-09 CrimsonHawk: SURFACE + REJECT — variadic-write dispatch cliff (keyed_values5-8) is REAL (+25-30% on 5-8v) but the cascade REORDER fix is ne | 1.094 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 21 | `NEGATIVE_EVIDENCE.md:4783`<br>2026-07-09 CrimsonHawk: KEEP SINTERCARD drop-loop guard (+3.5–5.9%); REJECT the same guard on full-scan SINTER/SUNION/SDIFF (0-gain) | 0.997 | ±2% — assumed: in-process instr shape | not visible in the live profile | no | VOID |
| 22 | `NEGATIVE_EVIDENCE.md:5417`<br>2026-07-09 CodexRedis: NO-SHIP — listpack span exact-capacity prealloc regresses restore decode — 0.93x release; code reverted | 0.93 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 23 | `NEGATIVE_EVIDENCE.md:5500`<br>2026-07-07 CrimsonHawk: REVERT (0-gain) — existing-key SADD into a packed generic set: rebuild bulk is MARGINAL/loses for the bounded packed range. Co | 0.99 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 24 | `NEGATIVE_EVIDENCE.md:6058`<br>2026-07-04 CrimsonHawk: NO-SHIP — OBJECT IDLETIME single-lookup collapse is ~1.02x vs ORIG, too small/noisy; reverted | 1.02 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 25 | `NEGATIVE_EVIDENCE.md:8170`<br>2026-07-04 CrimsonHawk: REJECT — small CompactFieldMap linear `contains_key` did not clear the SMISMEMBER gate | 0.982 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 26 | `NEGATIVE_EVIDENCE.md:8284`<br>2026-07-02 CrimsonHawk: REJECT — PUBLISH/SUBSCRIBE ~0.77x is SYSCALL-BOUND (delivery __send), NOT the channel-map SipHash; foldhash swap showed NO mea | 0.93 | ±10.5% — assumed: P16 multi-conn shape | not visible in the live profile | no | VOID |
| 27 | `NEGATIVE_EVIDENCE.md:8422`<br>2026-07-02 CrimsonHawk: MEASURED/REJECT — RDB SAVE is only ~1.4x vs redis 7.2.4 (NOT 3x — first read was cold/stale-process confounded), byte-size par | 1.01 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 28 | `NEGATIVE_EVIDENCE.md:9271`<br>2026-07-01 CrimsonHawk: REJECT — ChunkedList arena tail REGRESSES 0.63-0.87x end-to-end (CORRECTS the fe62b26ed micro-bench; move-vs-copy, not clone-v | 0.959 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 29 | `NEGATIVE_EVIDENCE.md:9585`<br>2026-06-28 BlackThrush: REJECTED PFMERGE missing-source no-reencode path — 1.007x vs ORIG / no stable Redis-ratio win | 1.007 | ±2% — assumed: in-process instr shape | not visible in the live profile | no | VOID |
| 30 | `NEGATIVE_EVIDENCE.md:9762`<br>2026-06-27 BlueFalcon: REJECTED medium-ZCOUNT count-only rank-tree warm gate — 0.963x vs ORIG | 0.963 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 31 | `NEGATIVE_EVIDENCE.md:10288`<br>2026-06-27 AmberRiver BITFIELD SET u8 aligned store fast-path rejected | 0.975 | ±10.5% — assumed: -c50 multi-conn shape | not visible in the live profile | no | VOID |
| 32 | `NEGATIVE_EVIDENCE.md:10680`<br>2026-06-25 BlackThrush 1-value keyed-write direct integer reply rejected | 1.006 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 33 | `NEGATIVE_EVIDENCE.md:10728`<br>2026-06-24 cod-b `frankenredis-uhthd` PFADD decoded-register cache rejected | 1.015 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 34 | `NEGATIVE_EVIDENCE.md:10965`<br>2026-06-21 cod-b `frankenredis-uhthd` batch list push helper rejected | 1.002 | ±2% — assumed: in-process instr shape | not visible in the live profile | no | VOID |
| 35 | `NEGATIVE_EVIDENCE.md:12238`<br>2026-06-20 cod-a bold-verify current refresh + rejected borrowed ZADD no-op shortcut | 1.0 | ±10.5% — assumed: -c50 multi-conn shape | not visible in the live profile | no | VOID |
| 36 | `NEGATIVE_EVIDENCE.md:12288`<br>2026-06-20 cod-a rejected list LP-byte reuse plumbing | 1.0 | ±2% — assumed: in-process instr shape | not visible in the live profile | no | VOID |
| 37 | `NEGATIVE_EVIDENCE.md:12328`<br>2026-06-20 cod-b rejected SMISMEMBER direct reply encoding | 0.99 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | yes | VOID |
| 38 | `NEGATIVE_EVIDENCE.md:12423`<br>2026-06-20 cod-a kept ZADD plain-owned store fast path; runtime-only shortcut rejected | 1.0 | ±10.5% — assumed: -c50 multi-conn shape | not visible in the live profile | no | VOID |
| 39 | `NEGATIVE_EVIDENCE.md:13140`<br>2026-06-21 cod-a `frankenredis-ohsk5` borrowed ListValue push helper rejected | 1.023 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 40 | `NEGATIVE_EVIDENCE.md:14349`<br>2026-06-22 (part 28) tcknm XADD side-map alloc fix — BYTE-EXACT but ~1.00x (mimalloc absorbs) — REVERTED (cc/BlackThrush) | 1.0 | ±10.5% — assumed: -c50 multi-conn shape | not visible in the live profile | no | VOID |
| 41 | `NEGATIVE_EVIDENCE.md:15047`<br>2026-06-25 (part 74) ZADD 3-member (*8) dispatch fast-path — ~0-GAIN, REVERTED (cc/BlackThrush) | 0.994 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 42 | `NEGATIVE_EVIDENCE.md:15060`<br>2026-06-25 (part 75) DEL/TOUCH 4-key fast-path SHIPPED (DEL4 1.57x, TOUCH4 1.39x); EXISTS4 ~0-gain REVERTED (cc/BlackThrush) | 0.994 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 43 | `NEGATIVE_EVIDENCE.md:16042`<br>2026-06-26 (part 131) MEASURED REJECT: insert_consumer fast-return — no demonstrable win on benchable path, REVERTED (cc/BlackThrush) | 1.03 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 44 | `NEGATIVE_EVIDENCE.md:16621`<br>2026-06-27 (part 172) NO-SHIP/SURFACE: length-audit CLEAN + remaining dispatch "gaps" are degenerate/noise (cc/BlackThrush) | 1.04 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 45 | `NEGATIVE_EVIDENCE.md:16734`<br>2026-06-29 NO-SHIP (unverifiable under load): ZRANK/ZREVRANK WITHSCORE combined rank+score one-pass (CrimsonHawk) | 0.99 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 46 | `NEGATIVE_EVIDENCE.md:16925`<br>2026-07-02 NEGATIVE (saturation re-confirm after 3 wins): command-throughput surface exhausted; residuals are dispatch-chain / peer-owned structural ( | 1.01 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 47 | `NEGATIVE_EVIDENCE.md:16951`<br>2026-07-02 BLOCKER (large-value SET zero-fill): 4MB SET 0.79x root-caused to a value-size memset, locked by #![forbid(unsafe_code)] + no-feature-gate  | 1.06 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 48 | `NEGATIVE_EVIDENCE.md:16972`<br>2026-07-02 REJECT (geo distance {:.4} i128 fixed-point): no measurable gain — low-precision uses fast grisu, not dragon (CrimsonHawk) | 0.895 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 49 | `NEGATIVE_EVIDENCE.md:19063`<br>2026-07-11 MEASURED-SUBGATE (list DUMP-encode single-parse fusion — byte-identical but no measurable win; REVERTED) — CreamPeak | 1.0 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 50 | `perf_negative_evidence_ledger.md:1163`<br>2026-07-15 CalmHeron: REJECT — fixed-shape canonical PSYNC2 CONTINUE reply parsing (`frankenredis-ac0uq`) | 1.00000051 | ±0.001% — row: 3x null CV 0.000188% | not visible in the live profile | yes | VOID |
| 51 | `perf_negative_evidence_ledger.md:1897`<br>2026-07-14 CalmHeron: NEGATIVE — NO-SHIP. cold compact-ZSET DUMP bitwise score classifier is null-drowned (`frankenredis-hdyw0`) | 1.0027 | ±5.9% — row: 2x |null median 0.9705 - 1| | not visible in the live profile | yes | VOID |
| 52 | `perf_negative_evidence_ledger.md:2184`<br>Rejected levers — measured REGRESSION or no-win (do NOT retry) | 1.01 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 53 | `perf_negative_evidence_ledger.md:3164`<br>MEASURED cod-b ohsk5 HSET direct histogram candidate (2026-06-20) -- REJECTED | not stated | ±10.5% — assumed: P16 multi-conn shape | not visible in the live profile | yes | VOID |
| 54 | `perf_negative_evidence_ledger.md:3225`<br>MEASURED cod-b 15lug residual CV confirmation + missing-key expiry candidate (2026-06-20) -- CANDIDATE REJECTED | 0.959 | ±10.5% — assumed: -c50 multi-conn shape | not visible in the live profile | no | VOID |
| 55 | `perf_negative_evidence_ledger.md:3460`<br>ZLEXCOUNT store-side micro-opt — DECLINED on measurement (BlackThrush 2026-06-20) | 1.07 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 56 | `perf_negative_evidence_ledger.md:4234`<br>2026-06-28 AmberRiver: Lua-map foldhash swap MEASURED ~0-gain (1.00-1.02x), REVERTED — hashing isn't the EVAL bottleneck | 0.996 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |
| 57 | `perf_negative_evidence_ledger.md:7954`<br>2026-07-10 cc_fr: NOT WIRED (revert-on-loss) — AVX2 `common_prefix_len` kernel wins 1.5–1.8x on ≥128 B in isolation, but routing LZF's hot path throug | 0.89 | ±20% — row: 2x |null median 0.9 - 1| | not visible in the live profile | no | VOID |
| 58 | `perf_negative_evidence_ledger.md:9272`<br>2026-07-11 cc_fr: MEASURED SUB-GATE — set/hash listpack decode intermediate-Vec elision (~1.05x, indistinguishable); reverted to stash | 1.039 | ±10.5% — assumed: era default (-c50/P16, +/-10.5%) | not visible in the live profile | no | VOID |

## Full audit

Machine-readable: `artifacts/optimization/campaign-20260725-cc/ledger_resurrection.json` (one object per audited entry).

---

## Reading the rehabilitation queue: the join is coarse

The self-time column is a **join**, not a per-entry profile. It matches an entry's backticked Rust
identifiers against the live profile's symbols, so an entry that merely *mentions*
`process_buffered_frames` inherits its 2.55%. Four rows tie at 2.55% for exactly that reason. The
hand-checked ranking below is by **target** frame — the thing the lever would actually change — and
is the list that was executed. Do not rank off the generated table alone.

A second, more important caveat, and the reason this audit's own numbers moved: an entry's target
frame can be **live in one workload and absent in another**. Rank #1 below targets the active-expire
clone loop, which the P16 GET profile used for the join scores at 0.38% — because
`redis-benchmark -t get` sets no TTLs, so the volatile-key set is empty and
`run_active_expire_cycle` early-returns. That is precisely the campaign's V3 predicate ("the
workload never routed through the code under test") turned on the audit itself. Ranking a
TTL-subsystem lever on a TTL-free profile would repeat the error the audit exists to catch.

## Hand-checked ranked top five

| # | Entry | Why it is void | What re-running it requires |
|---|---|---|---|
| 1 | `frankenredis-auer7` — active-expire live-key clone elision (`docs/perf_negative_evidence_ledger.md:1823`) | Measured **1.5787x** and rejected because the A/A null was p05..p95 **[0.834, 1.645]**, null median 1.0368, **null CV 54.66%**. A 58% effect on a substrate that cannot resolve 65%. | Its own retry predicate — *"revisit only with a stable instruction-counter substrate"* — is now satisfiable. The `--active-expire` arm of `benches/expire_reset.rs` was reverted with the candidate and must be rebuilt. |
| 2 | `write_i64_to_slice` direct-write (`docs/NEGATIVE_EVIDENCE.md:1881`) — the repo's only **GATE-VOID** | Measured **1.005097x** with effect CV **0.000011%**, null median 1.000000055, binary sha recorded, bit-identity proven. Rejected *only* by an `effect_median <= 1.01` keep gate. The measurement decided the lever; a round number vetoed it. | Re-add `benches/write_i64_to_slice_fastpath.rs` and re-adjudicate on the median-CI gate. No new design work. |
| 3 | LFU ZRANGEBYLEX member borrow-scan 2→1 (`docs/perf_negative_evidence_ledger.md:10364`) | Claimed **1.101** with a null recorded but no sha and no self-time; the ratio sits inside the ±10.5% era floor for its `-c50` shape. | Re-run on the `-c1` `instructions:u` shape (A/A cv ~0.02%). The `rng_seed` field-split lookup-collapse primitive it uses has ~15 prior lands, so the mechanism is proven elsewhere. |
| 4 | SET `drop_if_expired` guard (`docs/perf_negative_evidence_ledger.md:3762`) | Claimed **1.008** on a `-c50` shape whose A/A null floor is ±10.5% — the effect is ~13x below the floor. The row rejected its harness, not its lever. | Same `-c1` shape, where it is decidable by three orders of magnitude. |
| 5 | XADD `drop_if_expired` guard (`docs/perf_negative_evidence_ledger.md:4028`) | The same primitive on a different command, independently measured at **1.015x** and rejected on the same undecidable shape. | Run with #4. Landing on one should imply landing on both; landing on neither is a far stronger closure than either row currently carries. |

## Re-run results

| # | Status | Outcome |
|---|---|---|
| 1 | **Re-ranked, not re-run — and that is the finding.** | Profiling the workload that actually exercises it (`SET key val EX 100`, `-P16 -c50`, 100k-key steady state, 23,817 samples, fr DSO 45.24% of cycles) scores the active-expire clone loop at **0.38% of total cycles**. The lever is real and its rejection was void, but it is not where the TTL workload's cost is. The same profile instead attributed **~6% of total server cycles** to a previously unnamed structural defect: `used_memory` recomputed by a full keyspace scan on a 10 Hz timer, whose result is then discarded because `read_rss_bytes()` always succeeds on Linux. Full evidence, exact hunk, behavior-preservation argument and gate are in `docs/NEGATIVE_EVIDENCE.md` under *"OPEN LEVER … `used_memory` is recomputed by a FULL keyspace scan on a 10 Hz timer"*. Not implemented here: `crates/fr-store/src/lib.rs` is under an exclusive peer reservation; handed to the cod lane on thread `perf-campaign-20260725`. |
| 2 | **Re-run / REJECT** | The restored direct `write_i64_to_slice` candidate is byte-identical but now decisively worse: reference/candidate **0.979436579x** (about 2.10% more candidate instructions). A/A median **1.000000032**, bootstrap 95% median CI **[0.999999981, 1.000000153]**; exact reference self-time **14.48/16.42/15.71%**, zero lost samples; ELF `eba60f9…b036`. Production remains on tmp+copy. |
| 3 | **Re-run / KEEP** | LFU ZRANGEBYLEX 2→1 is decisive on the corrected substrate: reference/candidate **1.093620068x** (8.560% fewer instructions). A/A median **1.000013534**, bootstrap 95% median CI **[0.999978684, 1.000089043]**; exact reference self-time **13.10/13.32/14.71%**, zero lost samples; ELF `88f1a9c6…13f1`. The collapsed production path ships. |
| 4–5 | **Re-run / KEEP** | SET and XADD no-TTL expiry guards are decisive under the corrected harness: SET **1.043165328x** (4.138% fewer instructions), XADD **1.109453446x** (9.866% fewer). Their A/A bootstrap CIs are **[0.999999846, 1.000000383]** and **[0.999999843, 1.000001148]**; exact reference self-time is **13.06%** and **2.47%** median, zero lost samples; shared ELF `1ca99429…0fcc`. Both shipped forms stand. |

**Resurrection yield:** 195 audited · 58 void (57 VOID + 1 GATE-VOID, **29.7%**) · hand-checked
top five fully adjudicated: **3 KEEPs**, **1 REJECT**, and **1 profile-driven re-rank** that surfaced
a new structural lever at ~6% of total cycles. The corrected substrate also resurrected the
generated queue's shipped SSCAN clone elimination as a fourth KEEP. The lesson generalises: the
highest-value output of a ledger resurrection is sometimes the resurrected row, and sometimes the
profile you are forced to take in order to rank it.

---

## RE-AUDIT 2026-07-26 — frankenfs six-class taxonomy adopted verbatim

The fleet broadcast of 2026-07-26 replaced the ad-hoc scheme above with the
frankenfs taxonomy. Re-running both ledgers under it (`scripts/ledger_resurrection_audit6.py`):

| Class | Count |
|---|---:|
| Entries parsed | 1,233 |
| — KEEP | 638 |
| — SURVEY | 166 |
| — UNKNOWN (unparsable heading verdict) | 249 |
| **REJECT — audited** | **180** |
| VALID-AB | 23 |
| VALID-MECHANISM | 32 |
| VALID-PROFILE | 0 |
| **VOID-NONULL** | **124** |
| VOID-CV | 1 |
| VOID-ZEROSELF | 0 |
| **VOID total** | **125 / 180 = 69.4%** |
| Rows carrying a binary sha256 | 21 / 180 = 11.7% |

**Scoreboard line:** `frankenredis | 1233 | 180 | 125 | 69.4 | 5 | 3 | 1.1095`

Three things this changes or confirms:

1. **The CV gate is not the epidemic here either.** VOID-NONULL 124 against
   VOID-CV **1**, matching frankenfs's 214-vs-4 shape. The binary-sha rate also
   matches almost exactly (11.7% here, 10.9% there), which points at a fleet-wide
   era effect — 2026-06 prose rows written before null controls were adopted —
   rather than a per-repo discipline difference.
2. **`VALID-MECHANISM` moved the headline by 40 points, definitionally.** The
   ad-hoc audit above reported 29.7% void; this reports 69.4% on the *same*
   ledgers. Nothing new was discovered. The old scheme called "no null but a large
   claimed effect" sound-with-incomplete-provenance, where VOID-NONULL correctly
   calls a near-1.0 no-null row undecided; conversely VALID-MECHANISM correctly
   rescued 32 rows the old scheme would have voided, because this repo's
   convention is `instructions:u` A/Bs and an instruction ratio of ~1.00 does
   establish "no work was removed" without a null. **Cross-repo void rates are
   only comparable if every repo applied VALID-MECHANISM** — a repo measuring
   instructions will otherwise look worse than one measuring wall time.
3. **Ranking by target-frame self-time is impossible from these rows: 1 of 125
   void rows records one.** The workaround remains the live-profile join described
   above, which is what surfaced `frankenredis-va3z0` — the row ranked #1 scored
   0.38% on a TTL-free profile, and profiling the workload that actually routes
   through it found a 10 Hz discarded full-keyspace `used_memory` scan at 9.74% of
   total cycles instead. That was landed as `cc5d8dd18`.

## Institutionalization — the audit is now a gate, not an event

The broadcast's lesson is that ledger integrity **decays**: frankensqlite, which
audited once and then mechanically enforced the check, sits at 1.7%; repos that
audited once and moved on sit at 25-91%. So `scripts/perf_candidate_preflight.py`
makes all three integrity failures hard to commit:

```sh
# Grep both ledgers for a prior row on the proposed surface, print the row's
# concrete retry predicate when present, and stop until it has been read.
# exit 0 = clear; exit 2 = BLOCKED
scripts/perf_candidate_preflight.py check-candidate 'drop_if_expired'

# Refuse a NEW REJECT entry that records neither an A/A null nor a counted
# mechanism — the VOID-NONULL class, 124 of our 125.  exit 3 = REJECTED
#
# The same check refuses a NEW KEEP/SHIPPED entry without an explicitly labelled
# executing-binary/ELF SHA-256.  exit 4 = REJECTED
git diff --cached -U0 -- \
  docs/NEGATIVE_EVIDENCE.md docs/perf_negative_evidence_ledger.md \
  | scripts/perf_candidate_preflight.py check-entry -

# Install the repository-local plugin into the existing pre-commit chain runner.
# Installation refuses to overwrite an existing plugin.
scripts/perf_candidate_preflight.py install-hook
```

The active checkout installs the gate as
`.git/hooks/hooks.d/pre-commit/60-perf-ledger.py`; the existing pre-commit chain
runner invokes it for every commit that stages either ledger. The plugin passes
only the staged diff to `check-entry`, so an old unaudited row cannot satisfy a
new one. A mention such as “no A/A null” is deliberately not evidence, and a
source hash or commit hash is deliberately not a binary hash.

Both ledger modes normalise whitespace before matching, because the ledgers
hard-wrap at ~100 columns and a raw-text grep scores a wrapped `Null\nmedian` as
*no null recorded* — the first run of the original audit made exactly that
mistake and reported a 95% void rate against a true 69.4%.
