# Retry-predicate sweep — 2026-07-31 (BlackThrush)

Every negative-ledger row and every open bead carrying a retry predicate,
evaluated against today's state. **Rows whose verdict does not change are
listed too**; a sweep that only reports the ones that moved is a press release.

Reproduce the population with:

```sh
grep -ciE 'retry[ -](predicate|condition)' docs/NEGATIVE_EVIDENCE.md \
  docs/perf_negative_evidence_ledger.md
```

## Population, and why most of it is not owed a re-run

**116 distinct ledger rows** carry a retry predicate (164 raw matches; 116 after
de-duplicating headings that appear in both ledgers). Of those, **109 are
admitted results** — a KEEP or REJECT that already has its number — whose
predicate is a *standing reopen condition* of the form "reopen after X
semantics / parser / allocator / kernel / codegen changes." Those are not
blocked measurements and nothing is owed until the named surface changes.

**7 rows declare a blocked state** (HOLD / INVALID / CODE-ONLY). Those are the
only ledger rows a sweep can act on. Plus **14 open beads** carry a retry
predicate, all of them blocked measurements.

## The 7 blocked ledger rows

| row | predicate | verdict today |
|---|---|---|
| `NEGATIVE_EVIDENCE.md:118` — CODE-ONLY HOLD, P16 same-shard SET/GET queue envelopes | Frankensearch, FrankenPandas and FrankenFS each post `[trj] RELEASE`; then claim `trj-booking` exclusively and rerun 1/2/4/8/…/128 | **STILL BLOCKED.** `am robot thread trj-booking` returns *thread not found* from this project and the mailbox is empty, so no RELEASE can be confirmed and no CLAIM can be posted. Separately superseded in substance: `frankenredis-odusj` (b5628b580) showed the Amdahl ceiling is flat in W, so the curve cannot yield a competitive scaling claim at any W. |
| `NEGATIVE_EVIDENCE.md:159` — DIAGNOSTIC HOLD, pre-booking sharded routing | identical trj-booking predicate | **STILL BLOCKED**, same reason. |
| `NEGATIVE_EVIDENCE.md:25304` — INVALID-HOLD, two booked full-curve attempts failed the candidate CPU A/A gross-bias guard (`frankenredis-vag28`) | add a candidate-null-only preflight for W2 and W8, then on a quiet **exclusively booked trj** run three consecutive fresh 48-sample 24-order invocations with candidate wall and CPU null medians within 1% of 1.0 | **STILL BLOCKED.** Requires the trj lease, which is unobtainable (above). The fleet-alias repair described below does not help: trj is not an `rch` worker. |
| `NEGATIVE_EVIDENCE.md:3464` and `perf_negative_evidence_ledger.md:3186` — COMPETITIVE KEEP with saturated P1 HOLD (`frankenredis-ohsk5`) | re-run after changes to client sharding/pinning, io_uring ownership or CQ draining, event-loop output ordering, Redis incumbent, kernel, allocator, or release codegen | **NOT TRIGGERED.** The sharded-path commits since 2026-07-28 (`e7c77eae5` per-tick envelope fill, `b8894aff5` command-surface widening) touch the multi-worker sharded route, not the P16 single-executor SET route this row measured, and none of io_uring ownership, CQ draining, output ordering, the incumbent, the allocator or the release profile changed. Reading recorded so a later agent can dispute it rather than re-derive it. |
| `NEGATIVE_EVIDENCE.md:8156` — INVALID A/B, first SORT comparator ORIG was dead code | keep both historical `from_utf8` results observable to the optimizer with symmetric benchmark barriers, repeat the profile gate, require non-zero `from_utf8` self-time, and only then collect a ratio | **NOT SATISFIED — but not environment-blocked either.** This is unbuilt work, not a waiting measurement: nobody has written the repaired comparator. It should be a bead, not a ledger row waiting on a condition. |
| `perf_negative_evidence_ledger.md:14631` — FRONTIER / HOLD, INCR residual is the retained single keyspace lookup | reopen only if (1) the keyspace map/key representation or hasher changes, (2) a safe exact borrowed-lookup API can replace the true-hit byte comparison, (3) a fresh call-count profile shows >1 keyspace probe per successful INCR, or (4) a current profile names a different non-wrapper leaf at ≥5% self-time | **NOT TRIGGERED.** (1) the foldhash change that shipped was on the **client** map, not the keyspace map; (2) no such API exists; (3) and (4) no fresh INCR profile has been taken. |

**Net for the ledger: 0 of 116 rows have a newly satisfied predicate.** The ISA
provenance requirement and the corrected null doctrine — the two changes that
might have unblocked rows — turn out to have been applied *in place* when they
were made. `NEGATIVE_EVIDENCE.md:190-208` already carries the retroactive
re-adjudication under the corrected rule ("CI straddle is telemetry only … that
narrow one-sided CI is not by itself an invalid measurement"), and every row
that records ISA does so because the harness prints
`runtime_detected_isa=` at measurement time. There was no backlog waiting on
either.

## The 14 blocked beads — this is where the predicate did change

All fourteen share one predicate, written independently eleven times: *an
alias-correct RCH worker whose entire original cpuset stays below 20% busy
through preflight, with every slot reserved and no co-tenant job.*

The **alias-correct** half was unsatisfiable for a structural reason nobody had
named. `rch` declares `canonical_root = /data/projects`, `alias_root = /dp`.
Four workers had `/dp` pointing at `/data` (vmi1149989, vmi1227854, vmi1152480)
or `/data/tmp` (vmi1293453). `rch` hard-denies those with
`alias_wrong_target`, so every strict-remote benchmark was funnelled onto the
alias-correct workers — and per-CPU sampling with the harness's own predicate
(all CPUs ≤ 20% busy over a 500 ms window, 20 attempts) showed the four
*rejected* workers were the quietest in the fleet and the alias-correct ones the
busiest:

| worker | `/dp` before | 20-attempt preflight, 05:53 UTC |
|---|---|---|
| vmi1227854 | → `/data` (denied) | 13 / 20 clear |
| vmi1293453 | → `/data/tmp` (denied) | 13 / 20 clear |
| vmi1149989 | → `/data` (denied) | 12 / 20 clear |
| vmi1152480 | → `/data` (denied) | 9 / 20 clear |
| hz1 | → `/data/projects` (admitted) | 5 / 20 |
| vmi1167313 | → `/data/projects` (admitted) | 1 / 20 |
| hz2 | → `/data/projects` (admitted) | 0 / 20 |

Repaired with `ln -sfn /data/projects /dp` on all four. **Predicate now
satisfiable** — demonstrated, not asserted: `frankenredis-bodco` (ZRANGEBYLEX)
went from eleven consecutive attempts that never spawned a server to a clean
four-arm result at 2.1198x live Redis, landed in `3836c35ef`.

| bead | subject | status |
|---|---|---|
| `frankenredis-bodco` | ZRANGEBYLEX | **CLOSED — converted, 2.119766690x live Redis** |
| `frankenredis-vlrnn` | ZREVRANGEBYLEX | predicate satisfied; re-running |
| `frankenredis-va5me` | ZREMRANGEBYRANK | predicate satisfied; re-running |
| `frankenredis-5yhyh` | ZREMRANGEBYLEX | predicate satisfied; re-running |
| `frankenredis-in98j` | ZRANGEBYSCORE | predicate satisfied; re-running |
| `frankenredis-t7qgs` | ZREVRANGEBYSCORE | predicate satisfied; re-running |
| `frankenredis-bcva8` | ZREVRANGE | predicate satisfied; re-running |
| `frankenredis-wgrny` | LPOP/RPOP COUNT | predicate satisfied; re-running |
| `frankenredis-bj3mq` | ZDIFF / ZINTER 2-key | predicate satisfied; re-running |
| `frankenredis-fhjnd` | SSCAN/HSCAN/ZSCAN cursor-0 | predicate satisfied; re-running |
| `frankenredis-ox2xq` | ZMPOP 1-key MIN/MAX | predicate satisfied; re-running |
| `frankenredis-uld9l` | ZUNIONSTORE/ZINTERSTORE 2-key | predicate satisfied; re-running |
| `frankenredis-9601c` | BITOP AND/NOT | gate not yet committed; blocked on that, not on the fleet |
| `frankenredis-mixed-family-scaling-rerun-hgqyu` | mixed-family sharded sweep | predicate names "an alias-correct RCH worker" verbatim; **now satisfiable** |

## The constraint that remains, measured rather than asserted

The alias repair removes a permanent blocker but not a standing one: the
quiescence gate needs a quiet window and this fleet oscillates. The same seven
workers, re-probed with the identical predicate four hours later, returned
**0–1 of 20 clear**, worst-CPU 15–96% busy, with the load attributable to named
peer processes (`rustc`, `npm`, `du`, the `sbh` disk-pressure daemon) — not to
steal time, which measured `st=0` everywhere.

The practical consequence, from the four ZRANGEBYLEX invocations it took to land
one: **the gate is a lottery with roughly one-in-three odds per attempt during
fleet-busy hours**, and a run that clears the pre-measurement check can still
fail the post-measurement one. Each attempt costs about ninety seconds once the
target directory is warm, so the correct response is to retry the same pinned
invocation rather than to weaken `QUIET_CORE_PREFLIGHT_ATTEMPTS` or the 20%
limit. Discarded invocations must be disclosed in the entry, as they are in the
ZRANGEBYLEX row.
