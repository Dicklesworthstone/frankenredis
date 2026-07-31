# KEEP-class claim coverage audit — 2026-07-31 (CrimsonHawk)

Fleet audit. Policy 2 (2026-07-27) says a perf KEEP is campaign output only when
it carries a numeric FrankenRedis/Redis ratio from a harness that ran the LIVE
incumbent arm in the SAME invocation. That gate has been enforced on NEW entries
since it was written and had never been run BACKWARDS over the claims predating
it. Reproduce with `python3 scripts/claim_coverage_audit.py`.

**Nothing was deleted or weakened by this audit. It is an inventory.**

---

## Re-run 2026-07-31 (BlackThrush) at HEAD `3202339fb` — the three numbers, and zero movement

Re-run of the same tool against the same two ledgers after twelve conversion
gates were built. **The counts did not move.** They are reproduced here rather
than quietly re-stated, because the flat result is the finding.

| | count at re-run | after today's conversion |
|---|---:|---:|
| **KEEP claims held, total** | **568** | **569** |
| **carry a vs-incumbent ratio measured with the incumbent LIVE in the same invocation** | **41** (7.2%) | **42** (7.4%) |
| **do not** | **527** (92.8%) | **527** (92.6%) |

The middle column is the state this re-run found. The right-hand column is the
state after the ZRANGEBYLEX conversion recorded below — one claim added, one
claim supported. One.

The comparison the fleet asked for: FrankenFS found 67 of 186 (36.0%) with no
ratio. **Ours is 92.8% by the strict gate, 84.2% counting only entries with no
vs-Redis number of any kind.** That is worse than FrankenFS by a factor of two
to three, and it is the honest number.

### Cannot be converted vs not yet measured — these are different problems

Of the 527 unsupported:

| bucket | count | what it actually needs |
|---|---:|---|
| ratio **and** live arm present, written in a form the gate does not parse | 49 | reformat only; **no measurement** |
| no vs-Redis number anywhere, but an incumbent arm is constructible | 468 | **re-measure** |
| **structurally unconvertible — no incumbent arm exists for the surface** | **10** | **relabel `SELF-SPEEDUP`; never queue** |

The 10 unconvertible quantify only internal counters Redis publishes no analogue
of — instructions/op, syscalls/op, probe counts (`LFU HMGET 3 probes → 1`,
`dispatch-floor front gate N x fewer instructions`). No harness produces a
vs-incumbent ratio for them at any effort. Three currently sit at scorecard
tier, where a bare `1.4330x` reads as a throughput ratio against Redis when it
is a probe count against our own previous code. That mislabelling — not the
missing measurement — is their defect.

So the queue is **517 not-yet-measured (49 reformat + 468 re-measure)** and
**10 not-measurable**, not 527 of one kind.

### Why twelve gates produced zero conversions — root cause found today

Between `f690bcd37` and `3202339fb` I built live-incumbent dual-null gates for
the twelve highest-ranked Tier-1 claims (ZRANGEBYLEX, ZREVRANGEBYLEX,
ZREMRANGEBYRANK/BYLEX, ZRANGEBYSCORE, ZREVRANGEBYSCORE, ZREVRANGE, LPOP/RPOP
COUNT, ZDIFF/ZINTER, SSCAN/HSCAN/ZSCAN cursor-0, ZMPOP MIN/MAX,
ZUNIONSTORE/ZINTERSTORE). All twelve beads are still `in_progress` and **not one
ratio landed.** Every attempt died in the harness's host-wide quiescence
preflight (every CPU in the process cpuset ≤ 20% busy over a 500 ms window,
20 attempts) before a single server process spawned.

The cause was not load. It was a fleet topology defect. `rch` declares
`canonical_root = /data/projects`, `alias_root = /dp`; four workers had `/dp`
pointing at `/data` (vmi1149989, vmi1227854, vmi1152480) or `/data/tmp`
(vmi1293453). `rch` hard-denies those with `alias_wrong_target:/data`, so every
strict-remote run was funnelled onto the alias-correct workers — and by
measured per-CPU sampling those four rejected workers were **the quietest
machines in the fleet** (12–13 of 20 preflight samples clear) while the
alias-correct ones the scheduler kept choosing were the busiest
(vmi1156319 load 8.73/8 cores, 0 clear samples; hz2 0/20; vmi1153651 load 17.5).

Repaired 2026-07-31 with `ln -sfn /data/projects /dp` on all four; `rch
diagnose` now selects them. The unmeasurability was infrastructure, not
physics, and it had been reported as a per-bead retry blocker eleven separate
times without anyone naming the shared cause.

### First conversion landed: ZRANGEBYLEX, 2.1198x live Redis

With the fleet repaired, conversion-queue entry #1 was measured against a live
incumbent and **KEEPs**: `ZRANGEBYLEX` over a 2000-member equal-score skiplist
is **FrankenRedis/Redis 2.119766690x** wall (bootstrap 95% median CI
[2.071790051, 2.160818627]) and 2.099226619x CPU-per-fixed-work, four arms in
one invocation, both A/A nulls inside the 2% median guard, 97.4–97.8%
saturation, all three host-wide quiescence phases clear, ELF SHA-256s
self-reported from inside every process. Full entry in
`docs/NEGATIVE_EVIDENCE.md`. Three earlier invocations were discarded by the
harness (pre-pin quiescence; post-measurement quiescence; candidate CPU A/A
median 1.0226 > 1.02) and no number from them is quoted. Total: **569 claims,
42 supported.**

## Correction 2026-07-31 (BlackThrush): this audit's own ranking was wrong

Converting entry #1 exposed a bug in the tool that produced the queue above.
It is corrected in `scripts/claim_coverage_audit.py` and the numbers here are
restated. **The correction makes this repository's exposure look better, and it
is published for the same reason the harsh numbers were.**

`load_bearing_score()` decided whether a claim "surfaces in" a reader-facing
page. v1 accepted a bare **ratio token** — `1.18x` — found anywhere on that
page. Ratios are three or four characters from a tiny alphabet and scorecards
are full of them, so this matched collisions, not citations. ZRANGEBYLEX scored
120, *"in BOTH scorecards"*; **`ZRANGEBYLEX` appears in neither scorecard.** The
`1.18x` it matched belonged to an unrelated zset/hash RDB-decode row and an
unrelated `set`/`get`/`incr` table.

v2 replaced ratio-matching with subject-name matching and over-corrected the
other way: a claim about `SET key value EX` matched every page, because every
page says SET. That inflated 79 entries to the maximum score of 270.

v3, in force now: a bead id on the page is sufficient; otherwise the page must
**both** name the claim's subject **and** quote one of that entry's own ratio
tokens.

| | v1 (published earlier today) | v3 (corrected) |
|---|---:|---:|
| claims echoed in **both** scorecards | 47 | **18** |
| claims echoed in **one** scorecard | 32 | **24** |
| claims with any reader-facing exposure | 79 | **42** |
| claims at README tier | 0 | **0** |
| ledger-only, no reader-facing surface | 389 | **426** |

So the reader-facing exposure was **overstated by 88%** (79 → 42). The Tier-1
list in the ranked queue above — including the ZRANGEBYLEX/ZREVRANGEBYLEX/
ZREMRANGEBY* entries named as "highest-scoring examples, all in both
scorecards" — was largely an artefact of ratio collisions. Those claims are
still unsupported; they are just not on a page a user reads. The headline
counts (569 / 42 / 527) are unaffected: they come from the repo's own
`incumbent_measured()` gate, which this bug never touched.

## The one line

> **568 KEEP claims total. 41 carry a vs-incumbent ratio in the gate's declared
> form. 527 do not. Of those 527, 49 hold the evidence in an unrecognised format
> and need only reformatting, and 478 (84.2% of all claims) carry no
> vs-Redis evidence at all.**

## The breakdown, because one number hides the real problem

| bucket | count | what it means |
|---|---:|---|
| passes the gate predicate today | 41 | nothing to do |
| ratio + live arm, wrong format | 49 | **reformat** — minutes each, no measurement |
| quotes a vs-Redis ratio, no live-arm evidence anywhere | 111 | **re-measure** — highest risk (see below) |
| no vs-Redis number anywhere in the entry | 360 | **re-measure** from scratch |
| structurally unconvertible | 10 | no incumbent analogue exists (see below) |

The 92.8% headline is the strict-form number and it **overstates the problem**.
The 84.2% figure — claims with no vs-Redis evidence at all — is the one that
describes actual missing work. Both are printed by the tool; neither is the
flattering one alone.

The audit imports `perf_candidate_preflight.incumbent_measured()` rather than
reimplementing it. An audit that invented a looser definition of "supported"
would produce a comfortable number and prove nothing.

### Known limitation of the strict predicate, stated because it cuts our way

The gate matches a declared FORM (`FrankenRedis/Redis … N.NNx`). Older entries
routinely wrote the same evidence the other way round (`0.49x → 1.16x vs redis`).
Run against README.md's own three ratio claims, the strict predicate returns
`incumbent_measured=False` for all three — yet those claims explicitly state
"running side-by-side in the same invocation … A/A null control in the same
invocation (nulls 0.979–0.999)". So the strict count under-reports genuine
coverage, and any conversion pass should reformat before it re-measures.

## Where the exposure actually is: the scorecards, not the README

This is the audit's most useful finding and it was not the expected one.

**README.md carries only 3 ratio claims**, and they are the best-supported ones
we have: `XADD MAXLEN ~` at 1.668x / 1.733x with confidence intervals and null
controls, and the P16 paragraph, all of which name a side-by-side incumbent in
the same invocation. **No unsupported claim scored the README tier.** A user
reading the front door is not currently being shown an unsupported number.

The exposure is in the two scorecards:

| reader-facing surface | unsupported claims echoed |
|---|---:|
| README.md | **0** |
| `docs/perf_domination_scorecard.md` + `docs/RELEASE_READINESS_SCORECARD.md` (both) | 47 |
| one scorecard only | 32 |
| ledger-only, no reader-facing surface | 389 |

## Ranked conversion queue

Ordered by how load-bearing the claim is, because an unsupported claim a user
might act on is worse than one buried in a lab notebook. Full ranked list is the
tool's output; the tiers are:

**Tier 1 — 38 claims: quote a vs-Redis ratio, in a scorecard, with no live-arm
evidence at all.** Convert these first. This repository has three recorded false
positives of exactly this shape — io_uring 1.43→0.92, HGETALL 1.45→0.98, P16
1.49→1.33 — every one of which was a number that looked like a vs-Redis ratio but
had not been taken against a side-by-side arm. A reader cannot distinguish these
38 from the 41 that are real. Highest-scoring examples, all in both scorecards:

- ZRANGEBYLEX / ZREVRANGEBYLEX borrowed READ fast-paths (`0.49x→1.16-1.18x, BEATS redis`)
- ZREMRANGEBYRANK/BYSCORE/BYLEX fast-paths (`0.45x→~0.75x vs redis`)
- ZRANGEBYSCORE / ZREVRANGEBYSCORE READ fast-paths (`0.62x→parity+ vs redis`)
- LPOP/RPOP COUNT-form fast-path (`0.38x→0.76-0.78x vs redis`)
- ZDIFF / ZINTER 2-key READ fast-paths, BITOP, SINTERSTORE/SUNIONSTORE/SDIFFSTORE

**Tier 2 — 41 claims: in a scorecard, no vs-Redis number at all.** These are
self-speedups presented on a reader-facing surface. Lower risk than Tier 1
(a reader sees no incumbent comparison to misread) but they still occupy space
on a page whose purpose is incumbent comparison.

**Tier 3 — 389 claims: ledger-only.** Real work, correctly recorded, never
promoted to a reader. Convert opportunistically or leave; they mislead nobody.

## Not convertible — 10 claims

A different problem from unmeasured, and worth separating so nobody queues work
that cannot be done. These quantify only internal counters with no Redis
analogue: instructions per operation, syscalls per operation, probe counts.
Redis publishes no equivalent, so no harness produces a vs-incumbent ratio for
them. Examples:

- LFU HMGET no-field-TTL 3 probes → 1 (1.4330x fewer probes)
- LFU zero-copy LINDEX after key_type 2 probes → 1
- dispatch-floor front gates quoted as "N x fewer instructions"

These should be **relabelled** `Claim class: SELF-SPEEDUP` rather than converted.
Three of them currently sit at scorecard tier, which is the actual defect: a
"1.4330x" on a scorecard reads as a throughput ratio against Redis when it is a
probe count against our own previous code.

## What this audit does not claim

It classifies entries by the textual evidence they carry, not by re-running any
measurement. An entry could be perfectly measured and badly written (that is the
49) or well written and wrong. The output is a work queue, not a verdict on any
individual claim's truth.
