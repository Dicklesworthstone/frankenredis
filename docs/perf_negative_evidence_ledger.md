# Performance Negative-Evidence Ledger (frankenredis vs redis 7.2.4)

Purpose: stop the perf agents (cc, cod-b, CrimsonFalcon, …) from re-treading levers
already proven to NOT win, and record where the real residual gaps live + who owns them.
Append measured results; never delete a row — a "tried, didn't win" entry is the point.

Convention: ratios are fr/redis (>1.0 = fr slower / more RAM). "Measured" = ran a real
release A/B; "Reasoned" = algorithmic certainty without a release bench (cargo-check-only
turns). Keep claims honest — mark which.

## 2026-08-17 BrownIbis: KEEP (SELF-SPEEDUP) — one shared executor stops re-deriving a gate main.rs already caches: SADD **−10.30 pct**, LPUSHX **−9.52 pct**, RPUSHX **−9.49 pct**, and a write command on a DIFFERENT executor moves 0.1 instr/op (`frankenredis-ghmgp`)

Claim class: SELF-SPEEDUP. Campaign output: no. fr-before against fr-after; no incumbent arm ran
in these invocations and none is claimed.

RETRY PREDICATE, in brief here and in full below: measure LPUSH/RPUSH/SADD at the shapes
`redis-benchmark` actually issues before quoting this as a benchmark result; do not migrate another
route without auditing its callers for paths outside the borrowed batch; and take the remaining
executors in the order the bead records.

A/A NULL, same invocation, eight draws of the after ELF on the claim shape paired into four
ratios: **A/A null median 1.000878, bootstrap 95% median CI [0.999717, 1.002152]** (20,000
percentile resamples, seed 20260817). GATE: that bootstrap median-CI is the decision rule for this
row; an A/B inside the null's interval is refused however it reads. The A/B is 0.8970, about 85x
the null half-width from 1.0. **CV is diagnostic only and was never a gate here** — it is not
computed. Detail and the raw draws are in the A/A section below.

BINARIES (both `--base HEAD --clean-overlay`, HEAD `ca932b4f5` unmoved across both builds):

    before  bench_elf_sha256=69b53445e862017d77ea81b9f0f91b301b2de200f3667c83d18de8d7a50f433c
    after   bench_elf_sha256=3586dfc6bed3c79da32ccf593093d9469db97f5f33f5a9636dc217629798c864

SYMBOL-CHECKED, AND DISCRIMINATING THIS TIME: `nm -C` finds
`keyed_values_write_borrowed_with_default_write_gate` **0 times in the before ELF and 1 in the
after**. The same check was useless on my previous lever, which added only enum variants and match
arms; this one adds a function, so it fires. **A symbol check proves contamination when it fires
and proves nothing when it does not** — the general check is that the before arm's base is not an
ancestor of the lever.

### The frame is named, and my mechanism guess was wrong: `parse_borrowed_dispatch_floor_decimal` 34.0 -> 51.0

The row above hypothesised that threading `Option<bool>` changed inlining in a helper GET's route
shares. **That is wrong.** Reading the two dumps with `frame_delta.py --dispatch` — no new build,
the dumps were already on disk — every dispatch frame is flat except one:

    frame                                          before   after   delta
    process_buffered_frames                         185.0   184.0    -1.0
    classify_borrowed_dispatch_floor_packet_impl    112.0   112.0     0.0
    parse_borrowed_plain_set_bulk                    46.0    46.0     0.0
    parse_borrowed_plain_ping_packet                 35.0    35.0     0.0
    try_dispatch_floor_classified_action             34.0    34.0     0.0
    parse_borrowed_dispatch_floor_decimal            34.0    51.0   **+17.0**
    Runtime::parser_config                           11.0    11.0     0.0
    ---------------------------------------------------------------------
    dispatch total                                  457.0   473.0    +16.0

**`parse_borrowed_dispatch_floor_decimal` gained 50 pct of its own cost**, and it is the floor
classifier's digit parser — on the shared path of EVERY floor-classified command, not just GET.
That is corroborated by the per-command dispatch figures already in this row: GET 457 -> 473
(+16), ZADD 662 -> 681 (+19), SADD 617 -> 642 (+25). A regression in a helper GET happens to use
would not move ZADD and SADD too.

So the correct statement is stronger and narrower than the hypothesis it replaces: the getexgate
work costs **+17 instr/op in one named frame, paid by every command the floor classifier admits**.

WHY IT IS ALSO THE EXPLANATION FOR MY OWN SHRINKING NUMBERS: my routes are floor-classified, so
they pay it too. The clean pair measured my saving MINUS this, which is why -203/-204/-203 became
-180/-141.7/-146.7. It is one shared frame, not a per-route effect, which is why the shortfall was
roughly constant.

I have NOT diagnosed why that frame grew — the parser's own source, and whether getexgate added a
call site, changed a bound, or defeated an inline, is the owner's to read. Naming the frame is
where an outsider's contribution should stop; the fix needs the intent behind the change.

RETRY PREDICATE for whoever takes it: read `parse_borrowed_dispatch_floor_decimal` against
`5bc439a57^`, and if the growth is a new call site rather than a slower body, the cheaper fix is
to hoist the call rather than to speed the parser. Confirm with the same one-command
`frame_delta.py --dispatch` on a fresh pair; the frame is unambiguous, so no A/A is needed to see
a 17-instruction move in it.

### Where the plus-or-minus 20 came from: a GET dispatch cost in the getexgate work that shares this commit, and how it explains both pairs

The correction above bounded my lever at "somewhere between 142 and 204" and said no pair could
isolate it. A four-ELF comparison isolates it, and the answer changes what both rows should say.

**First, layout noise is ~1 instr/op, not 20.** Two independently built PRE-lever ELFs, different
bases with unrelated peer commits between them:

    shape             gh_before   clean_before   delta
    get_control        1303.0      1301.7        -1.3
    zadd_base          2729.8      2730.8        +1.0
    sadd_existing      1967.7      1967.6        -0.1

with dispatch IDENTICAL in all three (457, 662, 617). So a ~20 instr/op movement is not layout and
has to be explained.

**Second, the explanation.** `get_control` across all four ELFs — my lever is present in both
"after" arms, so anything that differs between them is not mine:

    ELF            contains                              get_control   dispatch
    gh_before      neither                                 1302.5        457
    gh_after       my lever + PARTIAL getexgate            1301.0        457
    clean_before   neither                                 1298.1        457
    clean_after    my lever + COMMITTED getexgate          1324.0        473

**My lever leaves GET alone: 1302.5 -> 1301.0 with dispatch flat at 457.** That is expected — it
touches `execute_plain_keyed_values_write_borrowed` and its floor dispatcher, neither of which is
on GET's path.

**The completed getexgate work adds +16 instr/op to GET's DISPATCH** (457 -> 473) and ~+26 to the
whole command. The +16 is an exact integer and reproduces; the total carries the usual few-instr
run-to-run wobble. My working-tree snapshot caught getexgate mid-flight — hence `gh_after` at 457
and `clean_after` at 473 with my lever constant across both.

### WHAT THIS DOES TO MY OWN NUMBERS

The clean pair's −180 / −141.7 / −146.7 are my saving MINUS the ~16 instr/op that the co-landed
change adds to every borrowed route's dispatch. Adding it back gives roughly −196 / −158 / −163,
which reconciles with the first pair's −203 / −204 / −203 to within a few instructions.

So the routes DO clear the 150 bar once the co-landed cost is attributed — **but that is an
inference from a subtraction, not a measurement of my lever alone**, and I am not restoring the
withdrawn LPUSHX/RPUSHX figures on it. They stay withdrawn until an arm exists with one lever and
not the other. What I will say is that the withdrawal was caused by a second change in the same
commit, not by my lever underperforming.

### WHAT THIS OBLIGES OF THE getexgate ROW, and it is the important half

**+16 instr/op on GET's dispatch is a regression on the most-executed command in the system, and
it is larger than the per-route saving that work claims for INCR and GETDEL.** It needs measuring
and either fixing or recording. The likely mechanism is that threading
`default_write_allowed: Option<bool>` through predicates changed inlining in a helper that GET's
borrowed route shares — an `Option<bool>` is not free where a `bool` was, and GET pays it without
using it.

That is a hypothesis, not a finding: the measurement is the +16, the mechanism is not established.
`scripts/frame_delta.py --dispatch` on the two dumps names the frame in one command and I have not
run it, because the row and the fix belong to whoever owns getexgate.

### RETRY PREDICATE

Reopen ONLY IF the caller-side key allocation is removed in the SAME change --
`KeyDict::insert` borrowing AND `Store::internal_entries_insert` no longer calling
`store_key_from_slice` -- so the per-key heap block is gone from BOTH sides. Until
that lands, the arena adds a copy without removing an allocation and must lose; the
measurement above is what that costs.

Three further conditions on any such retry:

1. The arena must grow in FIXED CHUNKS, never by doubling. On a key arena the
   doubling slack is per-key memory: +6.1 B/key measured here, which alone exceeds
   the win. A 1 MiB chunk caps it at one chunk for any keyspace size. Untested --
   the build never linked.
2. Re-measure the CONTROL on the same HEAD as the candidate. If the control is
   older than the candidate's HEAD the verdict is void: this pass's first control
   predated `hwcm1` and would have charged this lever 8.4 B/key that belongs to
   another.
3. Do NOT quote `UNACCOUNTED` from `keydict_byte_attribution_uhthd` as the expected
   saving. It is an upper bound on per-allocation overhead, not a prediction of what
   removing the allocation returns to RSS -- when the blocks went away here, 18.7
   B/key did not come back. Reopen on a measured whole-process delta only.

### WHAT STANDS

The attribution instrument itself is kept (it priced `hwcm1`'s prefilter at 8.4 B/key,
which nobody had costed in bytes/key) with its PAYLOAD-not-footprint caveat written into
the doc comment so the next reader cannot repeat mistake (1).

Keyspace stands at **1.8112x** vs live Redis 7.2.4 on this HEAD — up from 1.7612x
because of the prefilter's 8.4 B/key, not because of anything in this row.
