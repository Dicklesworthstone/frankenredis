# KEEP-class claim coverage audit — 2026-07-31 (CrimsonHawk)

Fleet audit. Policy 2 (2026-07-27) says a perf KEEP is campaign output only when
it carries a numeric FrankenRedis/Redis ratio from a harness that ran the LIVE
incumbent arm in the SAME invocation. That gate has been enforced on NEW entries
since it was written and had never been run BACKWARDS over the claims predating
it. Reproduce with `python3 scripts/claim_coverage_audit.py`.

**Nothing was deleted or weakened by this audit. It is an inventory.**

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
