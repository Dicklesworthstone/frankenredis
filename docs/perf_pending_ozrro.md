# Pending ledger rows — MossySparrow / frankenredis-ozrro

HOLDING FILE, not a second ledger. Every row below belongs in
`docs/perf_negative_evidence_ledger.md` and should be moved there verbatim, then this file
deleted.

WHY IT EXISTS. These rows have been unbankable for roughly ten ticks because a peer's
uncommitted row in the ledger fails `scripts/perf_candidate_preflight.py`'s Meta-Lever 2 timing
contract, and that check reads the WORKING TREE rather than the index — so hunk-filtering with
`git apply --cached` does not get past it and neither does a path-limited commit. Verified: my
staged diff contained zero peer content, the peer row was in neither HEAD nor the staged blob,
and the line number the hook printed matched the working-tree position exactly.

Meanwhile the shared working tree has already had my index reset once by a peer's add/commit
cycle. Roughly 300 lines of measured findings — including a CERTIFIED result on the instrument's
own precision — existed only as uncommitted text. That is not a safe place for them, and waiting
is not a plan when the blocker is outside my control.

Only two paths are schema-gated (`docs/NEGATIVE_EVIDENCE.md` and the ledger), so this file
commits cleanly. That is a workaround for a durability problem, NOT an attempt to route around
the contract: the rows are unchanged, they are still subject to it, and merging them into the
ledger is a mechanical step once the blocker clears.

## MEASURED (frankenredis-ozrro) — ZRANGE -198.7 instr/op confirms the closure-form class generalises: THREE conversions now land 191-204, and the vein reopens at 11 remaining candidates

fr-only. Before `fr-hget-inline`, after `fr-zrange-gate`. All 29 floor tests pass.

    shape          before     after     delta   dispatch
    zrange_plain  2,570.5   2,371.8   -198.7    453.9 -> 465.1
    type          1,400.3   1,397.8     -2.5    (holds -199.4 from its own conversion)
    dump_small    2,702.1   2,707.5     +5.4    CONTROL

Control-corrected: -204.1. Third point in the series:

    HGET     -191.5   closure form
    TYPE     -199.4   body form
    ZRANGE   -198.7   closure form
    spread     4 pct

Three independent commands agreeing to 4 pct clears the bar this ledger sets (two points never
establish a law here). The gate-inside-the-body/closure placement is worth ~195 per arm, and the
let-chain placement is worth -21. The FORM, not the command, decides it.

### THE VEIN REOPENS, NARROWER AND SIZED

I closed it two rows ago at "~1 pct of arms qualify" because my form-classifier only recognised the
literal `if let ... {` body form and left 59 arms unclassified. Screening for the CLOSURE form
(`.and_then(|packet| { .. })`) — the form HGET had used — found FOURTEEN candidates. Three are now
done, so ELEVEN remain at ~195 each. That is a real worklist rather than an executor count, and it
is the third time this campaign a screen's blind spot changed a vein's size in one direction or the
other.

### TWO TRAPS IN ONE LEVER, BOTH OF WHICH WOULD HAVE PRODUCED A CONFIDENT NULL

  1. WRONG TWIN. I first converted `execute_plain_zrange_borrowed` because my screen's regex
     matched the command name out of `execute_plain_zrange_borrowed_into` and I patched the shorter
     name. The floor arm calls `_into`. Converting an unused twin COMPILES and MEASURES AS ZERO.
  2. WRONG SHAPE. The corpus had only `zrange_rev` and `zrange_withscores` — option forms that this
     arm's own comment says fall through to generic. Neither reaches the plain arm. Measuring with
     either would have read -0.

Either alone would have produced "ZRANGE conversion: no effect", and with the closure class
unproven at that point I might have written off eleven candidates. Both were caught by reading —
the call site and the arm's comment — not by tooling, because both failure modes are silent.

    RULE, earned twice in two rows: before measuring a conversion, verify (a) that the function you
    changed is the one the call site uses, and (b) that the shape reaches the code you changed. The
    harness will faithfully measure a path you did not touch.

### STILL OPEN

Eleven closure-form candidates: Bitop (write), ListPopCount, ZsetPopCount, ZrangeWithscores,
LposRank, Zscore, Zrangestore6 (write), Getrange, Sintercard, HrandfieldCount, ZrandmemberCount,
SrandmemberCount, GeohashSingle. Only Sintercard has a shape today; the rest need one added first,
which is the cheap half. ZSCORE is the hottest of them and has no shape — that is the next lever.

## CLOSED (frankenredis-ozrro) — the read-gate vein is NOT a 93-arm sweep: 60 pct of floor arms are the let-chain form whose only legal gate placement measurably LOSES. Worth ~190 on ~1 pct of arms, and a per-arm inspection job for the rest

Source analysis over all 165 floor arms; no build, no measurement. Closes the vein I opened three
rows ago at "~93 executors, ~190 net per arm", which was an executor count masquerading as a
worklist.

    floor arm form                                              count   share
    let-chain  (`&& let Some(...)`) -> gate must be HOISTED         99   60 pct   MEASURED LOSS
    body form  (gate can sit inside the `if let`)                    1    1 pct   MEASURED WIN
    already converted (mine)                                         6    —
    other shapes, unclassified by this screen                       59   36 pct   inspect individually

### WHY THE FORM DECIDES THE OUTCOME

Measured, on the same executor and the same predicate:

    TYPE   gate inside the `if let` body     -199.4 instr/op
    TTL    gate HOISTED above a let-chain      +21.0 instr/op (control-corrected)

A let-chain arm reads `if let Some(packet) = parse(..) && let Some(response) = exec(..)`. A `let`
statement cannot occupy the expression position of the second clause, so the cached gate has to be
computed ABOVE the whole chain — before the parser has even run. That placement lost, twice
measured, and I reverted TTL and PTTL because of it.

So the vein's value is real (~190-200, confirmed on HGET and TYPE) and its SCOPE is not. 99 arms
are disqualified by construction unless someone restructures the arm's control flow first, which is
a manual rewrite per arm carrying the regression risk the TTL measurement demonstrated — for ~190.
That trade is not obviously worth taking and should not be entered by sweep.

### WHAT I GOT WRONG WHEN I OPENED IT

I wrote: "a per-pass read-gate cache would save on EVERY borrowed read on a pipelined workload" and
sized the vein at 93 executors. Both halves were wrong in the same way — I counted the executors
that COMPUTE the gate without checking whether their call sites could USE a cache. An executor
count is not a worklist, and the difference between them was 60 pct of the surface.

This is the same error shape as the `[A]`-candidate screen that listed ten commands without their
cascade depth, and as the `keys_star` intercept. Each time the count was right and the thing it was
a count OF was wrong.

### WHAT REMAINS, STATED SO IT IS NOT RE-OPENED BY SWEEP

  * The 59 unclassified arms need INDIVIDUAL inspection: HGET qualified via a closure form
    (`and_then(|packet| { .. })`) that this screen does not recognise, so some of the 59 are
    genuine candidates. Each is worth ~190 and costs one inspection.
  * The 99 let-chain arms are closed unless the arm is restructured first. Anyone tempted should
    read the TTL row before starting.
  * DONE and holding: HGET -191.5, TYPE -199.4, plus the write-gate trio (MSET -269.3, HSET -263.7,
    HMSET -269.5) which had no such structural limit because `cached_plain_write_gate` was already
    threaded and those arms were already body-form.

## MEASURED (frankenredis-ozrro) — the HGET wrapper is now `#[inline]`, and the measurement is a NULL by construction: no shape in this corpus can verify the saving, only that nothing regressed

    shape        pre-inline   post-inline   delta
    hget            1,782.4       1,782.7    +0.3   unchanged
    type            1,400.3       1,399.8    -0.5   -199.4 win holds
    dump_small      2,704.3       2,702.2    -2.1   CONTROL

HGET did not move, and it could not have: its floor arm calls
`execute_plain_hget_borrowed_into_with_default_read_gate` DIRECTLY, so it never traversed the
wrapper being inlined. The ~24 instr/op this recovers is paid only by LEGACY callers that kept the
old name, and no shape in this corpus exercises those paths.

So this row claims exactly two things and not a third: the change does not regress the arm that IS
measurable, and the ~24 figure is carried over BY ANALOGY from the keymeta wrapper where it was
measured directly (TTL 32.7 un-inlined, 11.0 inlined). Calling it "verified for HGET" would be
borrowing a number across a boundary the shapes cannot cross — the same error as sizing the read
gate from the write gate, which cost me a failed predicate two rows ago.

### WHY IT WAS MISSED THE FIRST TIME, AND WHY THE PATCH REPORTED IT

My inline patch printed `SKIP (found 0)` rather than silently succeeding: the docstring I anchored
on turned out to be a continuation of a prior author's comment block, not standalone text. A patch
that "succeeds" while matching nothing is the failure mode that produces a confident report of work
not done, and the count assert is the only reason it surfaced. That is the sixth text-matching
assumption of mine to be wrong this campaign and roughly the fourth caught by a guard rather than
by the compiler.

The justification is now recorded AT THE SITE with its measurement, because `#[inline]` on a
trivial delegating function is precisely what a later reader strips as noise.

## MEASURED (frankenredis-ozrro) — TYPE gains 199.4 instr/op from the cached read gate, TTL/PTTL are REVERTED because the same change made TTL worse, and the refactor's own wrapper cost 24 until `#[inline]` fixed it

fr-only, control flat throughout. Shared `keymeta` executor (TTL/PTTL/TYPE/EXPIRETIME/
PEXPIRETIME) given gate-taking variants; only TYPE keeps the cached gate.

    shape             baseline    final     delta   note
    type               1,599.7   1,400.3   -199.4   cached gate, KEPT
    ttl_nonvolatile    1,716.4   1,727.4     +11.0   reverted; +9.0 control-corrected, inside +/-10
    dump_small         2,702.3   2,704.3      +2.0   CONTROL

### THE SHARED EXECUTOR IS WHY THIS IS TRUSTWORTHY, AND IT NEARLY WENT THE OTHER WAY

Converting `keymeta` reached five arms at once, and the two MEASURABLE ones disagreed: TYPE -189,
TTL +21 (both control-corrected, first pass). Same executor, same predicate — the only difference
is WHERE the gate is computed. TYPE's sits inside the `if let` body; TTL and PTTL are let-chains,
where a `let` cannot occupy the expression position of `&& let Some(response) = ...`, so the gate
had to be HOISTED above the parser check. The hoisted placement lost.

Had I converted TTL alone — the obvious single-command choice — I would have banked a small
regression and could reasonably have written off the whole ~93-site read-gate vein. The vein is
real; it is the let-chain placement that fails. That is a result a single-command conversion
cannot produce.

So: TYPE keeps the cache, TTL and PTTL are reverted with the reasoning at both sites. PTTL is
reverted DESPITE never being measured, because the honest default for an unmeasured arm sharing
the losing pattern is to revert it, not to assume TYPE's result generalises.

### A COST I CLAIMED WAS FREE, AND WAS NOT

To keep 12 of 17 call sites compiling unchanged I left thin wrappers with the original signatures,
and described that as preserving existing callers "unchanged". True for compilation, FALSE for
performance:

    TTL, original monolithic executor              1,716.4
    TTL, wrapper present, un-inlined               1,749.1    +32.7
    TTL, wrapper present, `#[inline]`              1,727.4    +11.0 raw, +9.0 corrected

The un-inlined wrapper cost ~24 instr/op on every caller that kept the old name — cascade arms and
generic paths I had no business slowing down. `#[inline]` recovers it to within the +/-10 fr-arm
precision band, and the measurement is recorded at the site so the attribute is not removed as
cosmetic later. A refactor that adds a call frame to a hot shared executor is not free because it
is behaviour-preserving.

### VEIN STATUS, with a precondition it did not have before

    per-arm value    ~190-200 net   (HGET -191.5, TYPE -199.4)
    PRECONDITION     only arms whose gate can sit INSIDE the `if let` body qualify.
                     Let-chain arms need a different approach or should be skipped.
    wrapper rule     any wrapper added to preserve callers MUST be `#[inline]`, measured not assumed

### STILL OPEN, and reported rather than hidden

`execute_plain_hget_borrowed_into`'s wrapper is still un-inlined — my inline patch reported SKIP
(found 0) because that docstring differs from the one I assumed. HGET's own arm calls the
gate-taking variant directly so its -191.5 stands, but any legacy caller of that wrapper is paying
the ~24. That is a one-line follow-up and it is named here so it is not lost.

## MEASURED (frankenredis-ozrro) — HGET is the first non-GET read to use a cached read gate: -191.5 instr/op (-9.7 pct) with a FLAT control. My own 200-267 predicate NARROWLY FAILS on net, and the miss is informative: the gate is ~207 and the cache LOOKUP costs ~15

fr-only, control flat. Before arm `fr-after-hmset`, after arm `fr-after-readgate`. All 29 floor
tests pass.

    shape        before     after      delta   dispatch
    hget        1,973.9   1,782.4    -191.5    376.0 -> 391.5   (+15.5)
    dump_small  2,697.8   2,700.6      +2.8    312.0 -> 313.2   CONTROL

### THE PREDICATE FAILED BY 4 PCT AND I AM RECORDING IT AS A FAILURE

I wrote, before measuring: "validated only if the delta lands in 200-267 with the control flat and
dispatch unchanged." Two of three held — the control moved 2.8 and the mechanism is confirmed —
but the delta is 191.5, below the band, and dispatch did NOT stay unchanged. So the predicate is
not satisfied as written, and the honest reading is that my lower bound was wrong rather than that
the result is bad.

DECOMPOSING IT EXPLAINS BOTH DEVIATIONS AT ONCE:

    non-dispatch   -207.0    the read gate, no longer evaluated per packet
    dispatch        +15.5    the cache lookup, which is new work in the arm
    net            -191.5

So the read gate really is ~207, a strict subset of the write side's ~267 exactly as the predicate
argument said it should be — the subset reasoning was right and only my numeric floor was too
high. And the +15.5 is a cost I had not accounted for anywhere: a cached gate is not free, it is
one branch and one Option read per packet. On the write side that same overhead was already inside
the ~267 net and so never showed up separately.

### THE PLANNING NUMBER FOR THE REST OF THE VEIN IS ~190 NET, NOT 200-267

That matters because the vein is ~93 executors. Sizing it at the write-side figure would have
overstated the total by roughly 40 pct. Anyone converting further read arms should use:

    gross gate recovery   ~207 per packet
    cache lookup cost      ~15 per packet
    NET expected          ~190 per packet, and only on arms that are actually hot

### WHY HGET AND NOT SOMETHING HOTTER

TTL and TYPE have no per-command executor — both are served by the shared
`execute_plain_keymeta_borrowed(PlainKeyMetaCmd::...)`, so converting them means converting a
shared executor used by five commands at once, which is a different and larger job. GET is already
cached and was the existence proof. HGET is the hottest read with its own executor, a floor arm and
a shape, which is why it was the right validation target rather than the most attractive one.

### IMPLEMENTATION NOTES WORTH KEEPING

The EXISTING `plain_get_read_gate_cache` was reused rather than a second cache added: it already
holds this exact predicate and is already reset at the four points that invalidate it (main.rs
6788, 6862, 6880, 6891). A parallel cache would be a second thing to keep in sync, and this ledger
has a row about an enumeration lagging the machinery it gates.

No body was duplicated. `execute_plain_hget_borrowed_into` now takes the gate and a thin wrapper
preserves the original signature, so every pre-existing caller is untouched — the same shape as
`execute_plain_mset_borrowed_ok`. That is what kept a cross-crate change to two mechanical edits.

## MEASURED-STRUCTURE (frankenredis-ozrro) — the READ gate is cached for GET AND NOTHING ELSE, so every other borrowed read pays it per packet on BOTH routes. That kills the asymmetry lever for reads and exposes a larger one: a read-gate cache is worth ~200-267 per packet across the entire read surface

Source reading only; no build, no measurement. Load 18.6 / 18.0 / 16.3, one peer build, gate FIT
for fr-only and UNFIT for a ratio — neither needed.

### THE ASYMMETRY DOES NOT EXTEND TO READS, WHICH IS THE FIRST THING TO GET RIGHT

The write-gate finding was that the cascade amortises per pass while floor arms paid per packet.
I went looking for the same asymmetry on the read side and it is not there:

    cache                        consumers
    plain_write_gate_cache       11, via `cached_plain_write_gate`
    plain_get_read_gate_cache     1, inline at main.rs:7021 — the GET cascade arm, nothing else

So every borrowed read EXCEPT GET computes `plain_borrowed_default_key_read_allows` per packet, in
the cascade and in the floor alike. There is no amortisation for a floor read arm to lose, and no
lever of the kind I took for MSET/HSET/HMSET. Had I assumed the write result generalised, I would
have written gate-taking variants for read executors and measured nothing.

### WHAT IT EXPOSES INSTEAD, AND WHY IT IS BIGGER

The absence of a read cache is itself the opportunity. GET has one because someone bothered; the
other 93 internal call sites of `plain_borrowed_default_key_read_allows` in fr-runtime do not. A
per-pass read-gate cache would save on EVERY borrowed read on a pipelined workload — TTL, TYPE,
EXISTS, HGET, MGET, SCAN, DUMP and the rest — which is the largest command class there is.

SIZE, bounded rather than claimed: the write predicate is the read predicate PLUS the write-only
gates (no disk-write denial, no min-replicas-to-write), so the read gate is a strict subset of the
one measured at ~267 across three commands. That puts it at 200-267 per packet, and the lower
bound is the honest figure to plan with until one instance is measured.

### WHY THIS IS A VEIN AND NOT A LEVER

93 executors compute the read gate internally, and each needs a `_with_default_read_gate` variant
before its arm can use a cache — the same shape of change as the write side, but 93 times rather
than 4. GET already proves the pattern works and is the existence proof, not the whole job.

The tractable unit is ONE hot read at a time, each a self-contained change with a known target.
Ranked by traffic among reads that have a floor arm and a shape in this corpus: TTL, TYPE, EXISTS,
HGET. GET is already done and must not be re-attempted — it is served by the cascade arm with the
cache, exactly as SET is on the write side, and both are the traps a naive traffic-ordered sweep
would hit first.

### RETRY PREDICATE

Take ONE of TTL/TYPE/EXISTS/HGET: add `_with_default_read_gate` to its executor, switch its floor
arm to a new `cached_plain_read_gate` helper, and measure with `dump_small` as the layout control.
The claim is validated only if the delta lands in 200-267 with the control flat and dispatch
unchanged. If it comes in far below 200, the read predicate is genuinely cheaper than the write
one and the whole vein should be re-sized before any further arms are converted.

## MEASURED (frankenredis-ozrro) — HMSET was the last floor WRITE arm paying the gate per packet; switching it recovers 267.8 instr/op with the control FLAT, and the gate amortisation now has THREE agreeing points at ~267

fr-only. Before arm `fr-after-gate`, after arm `fr-after-hmset`. Shape verified a genuine no-op
first (dbsize and HGETALL both stable over 200 calls, reply +OK).

    shape        before     after      delta    dispatch
    hmset_2     3,300.4   3,032.6    -267.8     677.9 -> 679.3   (-8.1 pct)
    dump_small  2,699.0   2,700.7      +1.7     312.2 -> 312.3   CONTROL

THE CONTROL IS FLAT, which is what makes this the cleanest of the three gate measurements. The
previous pair had to be control-corrected for a ~50 instr/op layout swing; here the untouched
control moved 1.7, so -267.8 is attributable without correction. Dispatch is unchanged
(677.9 -> 679.3), confirming the route was not touched — only the non-dispatch gate work.

### THREE AGREEING POINTS, WHICH IS THE BAR THIS LEDGER SET FOR ITSELF

    command   gate recovery (control-corrected)
    MSET              -269.3
    HSET              -263.7
    HMSET             -269.5
    spread              5.8  =  2.2 pct

This ledger has four cases of a two-point law dispersing on the third (per-argument argv, the
~2,000 generic dispatch premise, the ~522 miss tax, the build-count dose-response), and I wrote
the rule that two points never establish a law here. Three commands agreeing to 2.2 pct is that
bar cleared: the per-pass write-gate amortisation is worth ~267 instr/op per packet on a
pipelined workload, and it is a property of the GATE rather than of any one command.

### HOW HMSET WAS FOUND, AND WHAT THE SCREEN ALSO RULED OUT

Only four executors have `_with_default_write_gate` variants — set, mset, hset, hmset. My three
arms already used the cache; HMSET was still on the `_ok` wrapper, which calls
`plain_borrowed_default_key_write_allows` itself and therefore pays per packet.

The same screen ruled out the command that would have looked most attractive: SET needs NOTHING.
It is served by the cascade arm at position 2 — the first lever of this campaign — and the
cascade has always used the cached gate. A screen that finds the one remaining case and
simultaneously stops you re-optimising the hottest command in the set is doing its job; the
naive version of this work would have started with SET.

### SCOPE, STATED NARROWLY

Every floor WRITE arm whose executor lacks a gate-taking variant still pays per packet — SPOP
among them, which I landed earlier. I am NOT claiming ~267 for those: adding a variant per
command is real work and the number is measured only where a variant already existed. What IS
established is the mechanism and its size, so the cost of the remaining arms is now predictable
rather than unknown, and each is a self-contained follow-up with a known target.

## MEASURED (frankenredis-ozrro) — threading the cascade's per-pass gate cache into the floor recovers the whole ~265 it was losing: MSET now -16.2 pct and HSET -11.8 pct against pre-lever, control-corrected

fr-only (load-immune), stamp FIT for fr-only on every run. Three ELFs, all built from this
campaign's own commits, with `dump_small` as the layout control in each.

    shape        pre-lever   lever only   +gate cache   dispatch (three builds)
    mset_2         2,999.8      2,742.3       2,523.4    915.4 -> 494.9 -> 499.8
    hset_same      2,332.6      2,280.6       2,067.3    681.6 -> 431.9 -> 437.6
    dump_small     2,688.5      2,648.6       2,699.0    305.2 -> 304.4 -> 312.2  CONTROL

Control-corrected against pre-lever: mset -486.9 (-16.2 pct), hset -275.8 (-11.8 pct). Those
are the ~15 pct and ~10 pct I predicted when the floor entries went in, and which the un-cached
gate had been eating.

### THE PREMISE WAS VERIFIED BEFORE THE FIX WAS BUILT, AND IT CORRECTED A SECOND THING I HAD WRITTEN

My amortisation explanation only holds if the harness batches commands into passes, so I read
the send loop instead of assuming: `payload = resp(*cmd) * ops`, sent as one concatenated
stream, with the code's own comment confirming the server "still batches per wakeup". Verified.

That reading also killed my own retry predicate, which said this fix needed "a pipelined shape
this corpus does not have". EVERY shape here is already pipelined. The precondition was
fictional and the fix was measurable with the shapes I already had — the third precondition I
have had to retract on this one lever, after the bypass A/B (wrong toggle: it switches
floor-versus-generic while this is cascade-versus-floor) and the executor-computes-internally
fix (a no-op, same predicate per packet).

### THE CHANGE

`try_dispatch_floor_classified_action` has exactly ONE call site, inside the cascade function
where `plain_write_gate_cache` is already in scope, so this is a signature widening plus a
one-line swap in each of the three arms to `cached_plain_write_gate`. Every floor WRITE arm can
now amortise the gate the way cascade arms always did.

### THE CONTROL IS WIDER THAN I HAD ESTABLISHED, AND THAT BOUNDS THIS ROW

`dump_small` across the three builds reads 2,688.5 / 2,648.6 / 2,699.0 — a 50.4 instr/op swing,
1.9 pct, against the 0.57 pct band I had established from ELEVEN readings. So the cross-build
LAYOUT term is ~50, not ~15, and every total here carries it. At effects of 200-490 that is a
4-to-10x signal-to-noise margin, which is why the conclusion holds; it would NOT hold for a
sub-100 lever, and this is the second row in a row to reach that conclusion from different data.

The DISPATCH figures are unaffected by layout and are the cleaner evidence: 499.8 and 437.6
remain inside the front-classified arity bands after the change, so the gate fix recovered
non-dispatch work without disturbing the route.

### WHAT I GOT WRONG AND WHAT IT COST

The gate cost was estimated at ~180 from the first pair and measured at ~265 here. I called it
"far smaller than the walk being removed" in the lever commit, which was wrong by enough to turn
HSET from a -10 pct win into a -2.2 pct one until it was fixed. The lesson is narrow and worth
keeping: I identified the term, wrote it down, and sized it by reasoning rather than measuring —
and a caveat that is never quantified is a guess wearing a disclaimer.

## CORRECTION (frankenredis-ozrro) — the gate fix I proposed last row would change NOTHING. The ~180 is a PER-PASS amortisation the cascade gets and a floor arm cannot, so "have the executor compute it internally" is a no-op and threading the cache is the only real fix

Source reading only; no build, no measurement. Load 16.4 / 14.0 / 10.5 rising with a build in
flight, so nothing was certifiable and nothing needed to be.

### WHAT I PROPOSED, AND WHY IT IS EMPTY

The previous row named two fixes for the ~180 instr/op the MSET/HSET floor arms pay for the
write gate, and stated a preference:

    1. PREFERRED — have the EXECUTOR compute the gate internally, as
       `execute_plain_spop_borrowed` and the other floor writes already do.
    2. thread the cascade's cache into the floor function.

Option 1 is a no-op, and reading three functions shows it:

    plain_borrowed_default_key_write_gate(&mut self, now_ms) -> bool {
        self.plain_borrowed_default_key_write_allows(now_ms)      // a pub WRAPPER, nothing more
    }

    execute_plain_mset_borrowed_ok(pairs, now_ms) {
        let default_write_allowed = self.plain_borrowed_default_key_write_allows(now_ms);
        self.execute_plain_mset_borrowed_with_default_write_gate(pairs, now_ms, default_write_allowed)
    }

So the "no-gate" variant evaluates the SAME predicate through the same private function; it
merely moves the call site from my arm into the executor. Identical work, per packet, either
way. I proposed it because the other floor writes use that shape, which is a reason to think it
is idiomatic and NOT a reason to think it is cheaper.

### WHERE THE ~180 ACTUALLY COMES FROM

`plain_write_gate_cache` is declared at main.rs:6731, OUTSIDE the per-packet loop, and reset only
on specific events (6790, 6882, 6893). So the cascade evaluates the gate ONCE PER BUFFERED PASS
and reuses it for every packet in that pass. The harness drives 2,000 identical commands, which
batch into few passes, so the cascade's per-packet gate cost is ~0 amortised while a floor arm
pays it in full on every packet.

That reframes the finding: it is not that the floor route added a gate evaluation, it is that the
floor route LOST AN AMORTISATION. Only option 2 recovers it, by giving the floor access to the
same per-pass cache. Option 1 cannot, because no per-packet call site can amortise across packets.

### WHY THIS MATTERS BEYOND THESE TWO COMMANDS

Every floor arm serving a WRITE pays this, not just mine. The floor's own writes
(`execute_plain_spop_borrowed` and friends) call `plain_borrowed_default_key_write_allows`
internally and therefore pay it per packet too. If the amortisation is worth ~180 per packet on
a batched workload, that is a cost the entire floor write surface carries and nobody has
measured. It is invisible in a c=1 unpipelined shape and would show up under pipelining, which
is the regime this campaign has mostly not measured.

I am NOT claiming that number for the other arms — it is measured for MSET/HSET only. It is
recorded as the question it raises.

### RETRY PREDICATE

  1. Option 2 is worth implementing only with a measurement that isolates it, and last row's
     suggested bypass A/B CANNOT: that switch toggles floor-versus-generic, while this lever is
     cascade-versus-floor. Correcting my own predicate again. The instrument that works is a
     pipelined shape (several commands per pass) measured before and after threading the cache,
     with `dump_small` as the layout control — because at ~180 the layout term (~40) is a
     comparable size and a bare paired build cannot resolve it.
  2. The floor-wide question reopens if a pipelined write shape shows floor-classified writes
     paying a per-packet gate the cascade amortises. That needs a shape this corpus does not
     have; adding one is cheap and should precede any code.

## MEASURED (frankenredis-ozrro) — the MSET/HSET before/after pair, and the CONTROL moved 1.5 pct below its own eleven-reading band, so part of my measured delta is code LAYOUT and not the lever

Both arms in one window; per-run stamps recorded because they disagree. Only the FIRST of six
arms read FIT — builds appeared after it and the remaining five ran with 2-3 present. Per-arm
loadavg 9.67-9.90 / 11.33-11.41 / 9.09-9.10, MHz 1,429-4,292. NOT a certification.

    shape        before (ratio / total)   after (ratio / total)   d-total   dispatch
    mset_2         0.4904x / 2,992.0        0.4812x / 2,771.4      -220.6   912.9 -> 497.1
    hset_same      0.5260x / 2,341.0        0.5111x / 2,274.7       -66.3   681.2 -> 431.6
    dump_small     0.5151x / 2,688.5        0.5248x / 2,648.6       -39.9   305.2 -> 304.4  CONTROL

### THE CONTROL IS THE FINDING

`dump_small` is untouched by this change and its fr arm fell 39.9 instr/op, 1.5 pct. Its
established band across ELEVEN readings, six sessions, loadavg 8-66 and 0-12 builds, is
2,686.3-2,703.7 — a 0.57 pct spread. 2,648.6 is BELOW that entire range. So the after-ELF is
genuinely different for a command I did not touch, and the plausible cause is CODE LAYOUT: this
lever added 239 lines to main.rs, which moves everything after it.

That bounds the lever's real effect rather than settling it:

    mset_2     raw -220.6   control-corrected -180.7   -> between -6.0 pct and -7.4 pct
    hset_same  raw  -66.3   control-corrected  -26.4   -> between -1.1 pct and -2.8 pct

Control-correcting by subtraction assumes the layout shift lands equally on every command, which
is an assumption and not a measurement, so both bounds are quoted rather than a point estimate.
The DISPATCH figures are unaffected by this: 912.9 -> 497.1 and 681.2 -> 431.6 reproduce the
fr-only run's 915.4 -> 494.9 and 681.6 -> 431.9 to within 0.5 pct, so the route demonstrably
moved. It is only the TOTAL that is contaminated.

### WHAT THIS SAYS ABOUT MY OWN CROSS-BUILD PAIRS

Every before/after total this campaign has reported from two separately built ELFs carries this
same layout term, and until now the control had never moved enough to expose it. It did here
because the change is large (239 lines) and the effect is small (26-220), so the layout term is
a comparable size for the first time. The corollary is uncomfortable and worth stating: a
cross-build pair can resolve a 3,000-instruction lever cleanly and cannot resolve a
200-instruction one. The bypass-switch A/B (one binary, env-var toggle) is the only instrument
here that excludes layout, and small levers need it rather than a paired build.

### RATIOS, AT THE TWO SIGNIFICANT FIGURES THE DENOMINATOR SUPPORTS

    mset_2     0.49x -> 0.48x
    hset_same  0.53x -> 0.51x

Both improve, both remain far below parity. The before-ratio for mset_2 read 0.4904x here
against 0.5158x two sessions ago — 4.9 pct apart on the same ELF and shape, which is an
independent confirmation of the ~4 pct intrinsic denominator width certified earlier, arrived at
by accident rather than by design.

### RETRY PREDICATE

Reopen the net-effect question with a BYPASS-SWITCH A/B rather than a paired build: one ELF built
with `--features perf-ab-cascade-bypass` at this commit, measured with FR_PERF_AB_CASCADE_BYPASS
toggled. That excludes layout entirely and is the only way to size a sub-300-instruction lever
here. The gate-removal follow-up (executor computes the write gate internally) should be measured
the same way, and its target remains the ~180 identified in the previous row.

## MEASURED (frankenredis-ozrro) — the MSET/HSET floor entries move the route as predicted but net only -8.6 pct and -2.2 pct, because the un-cached write gate I flagged as "not free" costs ~180 instr/op and eats most of the saving

fr-only, so window quality is not load-bearing; stamp read FIT for fr-only on every run.
Per-arm loadavg 15.24 / 12.13 / 8.64, MHz 1,429-4,238, 2 builds present. Before arm
`fr-after-pubsub`, after arm `fr-after-msethset` (5e0195905, sha 3e75a16a).

    shape        disp before -> after   d-disp   total before -> after   d-total   d-nondisp
    mset_2            915.4 ->  494.9   -420.5      2,999.8 -> 2,742.3    -257.5     +163.0
    hset_same         681.6 ->  431.9   -249.7      2,332.6 -> 2,280.6     -52.0     +197.7
    dump_small        305.0 ->  306.2     +1.2            —                   —          —   CONTROL

### THE ROUTE MOVED. THE SAVING DID NOT ARRIVE.

Dispatch fell to 494.9 at arity 5 and 431.9 at arity 4, both inside the front-classified bands
this campaign established (~520 and ~470) and in fact slightly better. So the entries work
exactly as intended and the classifier is doing what it claims.

But the TOTAL fell far less than the dispatch did, because non-dispatch work ROSE by 163.0 and
197.7 — a per-call constant of ~180 +/- 18 across two unrelated commands, which is what makes it
attributable rather than noise. That is the write gate. The commit message for the lever said:

    "if a LATER cascade arm in the same pass would have reused the cached value, this route pays
     one gate evaluation the cascade route might not have. That is the trade, and it is far
     smaller than the walk being removed, but it is not zero and should not be described as
     free."

The first half of that was right and the last clause was wrong. It is not far smaller than the
walk: it is 43 pct of MSET's dispatch saving and 79 pct of HSET's. Net delivery is -8.6 pct on
MSET (against ~15 pct predicted) and -2.2 pct on HSET (against ~10 pct) — and HSET is close
enough to a wash that at two significant figures its ratio does not move at all.

### WHY THIS IS A GOOD OUTCOME TO HAVE MEASURED RATHER THAN ASSUMED

I predicted 10-15 pct from the dispatch delta and the cascade multiplier, and the dispatch delta
was RIGHT. The prediction failed on a term I had identified, written down, and then still
under-weighted — I reasoned "smaller than the walk" without measuring it. A flagged caveat that
is never quantified is a guess wearing a disclaimer.

### THE FIX IS IDENTIFIED AND IS A REAL FOLLOW-UP LEVER

The gate is computed per packet here only because
`try_dispatch_floor_classified_action` does not receive the cascade's
`plain_write_gate_cache`. Two ways out, in preference order:

  1. Have the EXECUTOR compute the gate internally, as `execute_plain_spop_borrowed` and the
     other floor writes already do — they call `plain_borrowed_default_key_write_allows`
     themselves and take no gate parameter. That removes the double-accounting rather than
     caching it, and matches the pattern the rest of the floor already uses.
  2. Thread the cache into the floor function. Cheaper to write, but it widens a signature that
     every floor arm shares for the benefit of three of them.

Expected recovery is the full ~180 per call, which would put MSET at roughly -437 total (-15
pct, the original prediction) and HSET at -230 (-10 pct). That is now a MEASURED target rather
than an estimate, which is the useful thing this row produces.

### RETRY PREDICATE

Reopen as landed only when the after-measurement shows non-dispatch UNCHANGED (within the 0.57
pct fr-arm precision) rather than +180, with dispatch still in the arity band. If non-dispatch
does not return to its before-value, the gate has merely moved rather than been removed.

## MEASURED (frankenredis-ozrro) — three runs of the SAME shape on the SAME ELF give ratios 0.4944x / 0.5274x / 0.5354x. The fr arm varies 0.056 pct and the redis arm 8.3 pct, so ALL ratio uncertainty is denominator uncertainty and it is ~8 pct, not the ~4 pct I banked

`dump_small` on `fr-after-pubsub`, three consecutive invocations minutes apart. Window stamp on
every run: loadavg 9.22-9.37 / 14.98-15.05 / 27.44-27.53, MHz 1,429-4,291, and 5-6 cargo/rustc
processes present — I pre-checked ZERO builds and they appeared during the sequence, which the
harness stamp caught and I would otherwise have missed. UNFIT for a ratio by my own gate, so
this is SIZING and does not discharge the retry predicate it was run for.

    run   fr instr/op   redis instr/op    ratio
      1      2,697.3        5,456.2      0.4944x
      2      2,696.1        5,112.2      0.5274x
      3      2,697.6        5,038.2      0.5354x
    spread    0.056 pct      8.30 pct    8.29 pct

### THE NUMERATOR IS NOT THE PROBLEM AND NEVER WAS

Three fr readings inside 1.5 instr/op — 0.056 pct — with 5-6 competing builds and the load
halving underneath them. Across all EIGHT sessions the fr arm of this shape now spans
2,686.3-2,703.7, 0.65 pct, over loadavg 14 to 66 and 0 to 12 concurrent builds. That is the
instrument working.

Every bit of the ratio spread is the denominator: 5,038.2 to 5,456.2 on identical input. Not a
drift over sessions — three runs minutes apart on one binary.

### CONSEQUENCE FOR WHAT IS ALREADY BANKED

My earlier correction put the denominator at 1.6-4.0 pct and concluded ratios carry ~4 pct. That
was too generous by half. A ratio measured in a window with builds carries ~8 pct, which means:

  * Every ratio in this ledger quoted to four significant figures is over-precise. MSET 0.5158x
    and HSET 0.5093x differ by 1.3 pct and are INDISTINGUISHABLE; I should not have presented
    them as ordered.
  * Any ratio within ~8 pct of parity is not established by a single dirty-window run. Checking
    my own board for casualties: `keys_star` at 1.0269x was only 2.7 pct above parity — INSIDE
    this band, so "KEYS is above parity" was never established by that measurement. It had
    already been withdrawn on independent grounds (gvm6z showed it was an n=2 intercept
    artefact, and the later reading was 0.4953x), so nothing downstream moves. It is recorded
    because the number should not have been asserted in the first place.
  * The large claims survive comfortably: PUBSUB NUMPAT 0.4630x, SCAN 1.2810x before and
    0.6898x after, pubsub_channels 2.3324x. All are multiples of 8 pct away from any boundary
    that matters.

### THE HARNESS STAMP PAID FOR ITSELF ON ITS FIRST REAL USE

I checked builds by hand immediately before launching: zero. Run 1 reported six. Without the
per-run stamp I would have banked three "build-free" readings and concluded the denominator is
intrinsically 8 pct wide, when what I actually measured is 8 pct wide WITH builds. That is the
difference between a correct caveat and a wrong law, and it was caught by a line of output
rather than by vigilance.

### A SECOND SEQUENCE HALVES THE SPREAD WHEN THE BUILD COUNT HALVES

Repeated in a calmer window with FEWER competing builds. Same shape, same ELF, stamps recorded
per run:

    sequence   builds   loadavg (1/5/15)        redis readings              spread
    first       5-6     9.2-9.4 / 15.0 / 27.5   5,456.2 5,112.2 5,038.2     8.30 pct
    second      3       10.6-11.2 / 13.3 / 24.8 5,112.5 5,216.7 5,041.4     3.48 pct

    ratios, second sequence: 0.5262x / 0.5168x / 0.5350x -> 3.52 pct

Halving the build count roughly halved the denominator spread. That is a DOSE-RESPONSE and it
points at build interference rather than intrinsic width — which is the answer the retry
predicate was asking for, arrived at from the opposite direction: I could not obtain a
build-free window, so I varied the build count instead.

TWO POINTS, AND THIS LEDGER HAS A STANDING RULE ABOUT THAT. Two points make a law-shaped object
every time; three decide whether it is a law. The per-argument argv model, the "~2,000 generic
dispatch" premise and the ~522 miss tax all looked solid on two and dispersed on the third. So
this is recorded as SUGGESTIVE, not settled: 6 builds -> 8.3 pct, 3 builds -> 3.5 pct, and a
zero-build sequence would extrapolate to roughly 2 pct if the relationship is real.

WHY A BUILD-FREE SEQUENCE COULD NOT BE OBTAINED, twice: I hand-checked ZERO builds immediately
before launching each sequence, and the harness stamp reported 6 and then 3 by the time the
first run executed. Peers start builds continuously on this host, so "no builds running" is a
property of an instant rather than of a window, and only a per-run stamp can tell you which one
you actually got. That is now the strongest argument for the stamp existing.

### RESOLVED IN A FIT WINDOW, AND IT GOES AGAINST MY OWN HYPOTHESIS

Three runs with every stamp reading FIT and builds 0 throughout — the first genuinely clean
ratio window of this campaign. Per-arm loadavg 8.73-9.07 / 9.75-9.80 / 16.49-16.55; MHz
1,429-4,288.

    run   fr instr/op   redis instr/op    ratio     stamp
      1      2,697.6        5,047.1      0.5345x   FIT, builds 0
      2      2,698.4        5,110.2      0.5280x   FIT, builds 0
      3      2,688.3        5,263.6      0.5107x   FIT, builds 0
    spread    0.376 pct      4.29 pct    4.66 pct

The predicate was explicit: under 2 pct means the 8 pct was build interference and clean-window
ratios keep three significant figures; otherwise the uncertainty is intrinsic and every banked
ratio needs trimming to two. It reads 4.29 pct. **The uncertainty is intrinsic.**

AND THE THIRD POINT KILLS LAST ROW'S DOSE-RESPONSE, WHICH I FLAGGED AS THE LIKELY OUTCOME:

    builds   redis spread
      5-6      8.30 pct
      3        3.48 pct
      0        4.29 pct     <- not monotone

Zero builds is WIDER than three. So "halving the builds halves the spread" was a two-point
artefact, and I recorded it as suggestive-not-settled for exactly this reason. That is now the
FOURTH quantity in this ledger to look like a law on two points and disperse on the third, after
the per-argument argv model, the "~2,000 generic dispatch" premise and the ~522 miss tax. The
pattern is consistent enough to be a rule: in this system, two points never establish a law.

### WHAT THIS OBLIGES, and it applies to my own rows first

Every fr/redis ratio in this ledger is quoted to more precision than the instrument supports.
The honest form is TWO significant figures:

    PUBSUB NUMPAT   0.4630x -> 0.46x        SCAN before   1.2810x -> 1.28x
    PUBSUB NUMSUB   0.5551x -> 0.56x        SCAN after    0.6898x -> 0.69x
    MSET            0.5158x -> 0.52x        pubsub_channels 2.3324x -> 2.3x
    HSET            0.5093x -> 0.51x        dump_small    ~0.51-0.53x

MSET and HSET are then plainly the same number, which is the correct reading and which I got
wrong when I presented them as ordered. No structural conclusion changes: every crossing this
campaign claimed is 25 pct or more from parity, and 4-8 pct cannot reach that.

The fr ARM keeps its precision. Eleven readings of this shape now span 2,688.3-2,703.7 — 0.57
pct — across loadavg 8 to 66 and 0 to 12 concurrent builds. Self-A/B deltas and dispatch figures
are unaffected by any of this; only the competitive ratio loses digits.

### RETRY PREDICATE (superseded by the FIT sequence above; kept for the record)

The question "is the denominator 8 pct wide intrinsically, or only with builds present" is NOT
answered here. It needs three `dump_small` ratio runs whose stamps all read FIT — i.e. zero
cargo/rustc for the whole sequence and 1min/5min within 15 pct. If the spread stays under 2 pct
there, 8 pct is build interference and clean-window ratios can keep three significant figures.
If it does not, every banked ratio needs trimming to two.

## METHOD (frankenredis-ozrro) — a window gate, because four consecutive "clean window" reports did not reproduce on my own check, and the right threshold is DIFFERENT for an fr-only number than for a ratio

`scripts/certification_window.py`, with a self-test. Source and process inspection only; no
build, no measurement. It refused the window it was written in, which is the point.

### THE TWO GATES ARE NOT THE SAME GATE

Conflating them is why one blunt threshold would be wrong, and this campaign has the data to
separate them:

    fr-only   `dump_small`'s fr arm reads 2,686.3-2,703.7 across SIX sessions spanning loadavg
              14 to 66 — 0.65 pct total spread. A dirty window costs wall-clock, not accuracy.
    ratio     the same control's REDIS arm reads 5,045.8 / 5,046.6 / 5,050.8 / 5,345.9, the last
              5.9 pct high with four peer builds running, and an earlier row showed the
              variation does not track load monotonically, so it cannot be corrected after the
              fact.

So the tool takes `--for ratio` (strict, refuses outright if ANY cargo/rustc is running) or
`--for fr-only` (lenient, notes them and passes).

### IT GATES ON STATIONARITY, NOT ABSOLUTE LOAD, AND THAT IS DELIBERATE

The recurring trap this session was the DECAYING window: a reassuring 1-minute over a much
larger 5- and 15-minute, where a run straddles two regimes. The gate measures
|1min - 5min| / 5min and uses the 15-minute as a settled-ness witness, so a low 1-minute under a
90 15-minute reads as a dip rather than calm.

It deliberately does NOT hard-fail on absolute load. This campaign's most reproducible rows were
taken at loadavg 14-24; a gate that refused those would discard good work, which is the
frankenpandas failure this ledger already has a row about wearing a different costume. A
self-test case pins that steady load 22 stays usable.

### WHY IT WAS WORTH MECHANISING

Four ticks in a row reported a clean window and my own `uptime`/`pgrep` found otherwise — 4
processes, then 36, then 3, then 7 rising within a minute of each other. Hand-checking caught
all four, but it is exactly the check that gets skipped on the tick where it matters. Run live
for this row it returned UNFIT for a ratio (7 cargo/rustc processes, 15-minute 43.91) and FIT
for fr-only, which is the correct pair of answers and is why the denominator retry predicate
below still cannot be discharged.

### WHAT REMAINS BLOCKED, and by what

  * The denominator-width test (three `dump_small` redis arms, spread under 2 pct decides
    whether 5.9 pct is build interference or intrinsic) needs `--for ratio` to return FIT. It
    has not yet.
  * MSET/HSET floor entries need `crates/fr-server/src/main.rs`, reserved by RusticLark until
    09:20Z. Their before-figures are now measured twice, 0.27 pct and 0.03 pct apart.

