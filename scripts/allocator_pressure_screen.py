#!/usr/bin/env python3
"""Rank borrowed executors by allocation sites, and flag the ones on a MISS path.

Cross-project signal (frankenlibc, frankentorch): both independently reidentified
the allocator as the dominant primitive -- libc at 10-16x against printf's 3.2x, and
torch with zero of 21 lanes certifying and null failures tracking load. The shared
tell is VARIANCE: allocator cost depends on heap state, so an allocation-heavy route
moves run to run while a lean incumbent holds steady.

frankenredis shows the same signature on its worst measured route. MOVE's own arm
swung 15% between 100-sample runs (102.73 -> 117.92 us) while redis-7.2.4 held to 1%
(66.221 -> 65.589, criterion reporting no change at p=0.57), with 12% outliers against
redis's 2%.

Reading execute_plain_move_borrowed found the mechanism, and the ORDERING is the
interesting part:

    47  if !exists_no_stat(source)  -> Integer(0)      <-- miss returns here
    50  let destination = encode_db_key(target_db, key)   allocation, correctly AFTER
    71  let key_owned = key.to_vec();                     allocation, UNCONDITIONAL
    72  let db_owned  = db_arg.to_vec();                  allocation, UNCONDITIONAL

Lines 71-72 sit after the reply is computed, so a MISS still pays them. They exist to
feed a propagation closure that a miss never invokes. Upstream moveCommand on a miss
does lookupKeyWrite, gets NULL, and calls addReply(czero) -- zero allocations.

So this screen counts allocation sites per borrowed executor and, separately, how many
of them sit after an early `Integer(0)` / `None` return -- the ones a no-op call still
pays for. Those are the cheapest wins: hoisting them behind the condition changes no
behaviour.

It is a SCREEN, not a measurement. A count is not a cost: a Vec of two bytes and one
of two megabytes both count once, and mimalloc makes small reuse cheap
(frankenredis-feedback_mimalloc_defeats_buffer_reuse_levers). Rank with it, then
measure.

  python3 scripts/allocator_pressure_screen.py [--top 15] [--self-test]
"""

import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Observed 2026-08-16: MOVE, WAIT, bitfield_get, zcount, bitcount.
NOOP_BASELINE = 5
FR = os.path.join(REPO, "crates/fr-runtime/src/lib.rs")

ALLOC = re.compile(r"\.to_vec\(\)|\.clone\(\)|format!\(|vec!\[|String::from|to_string\(\)"
                   r"|Vec::with_capacity|\.to_owned\(\)")
# NOT `return None;` -- every borrowed executor opens with a policy-gate
# `return None;`, so including it made "after" match "total" for essentially every
# row and the column discriminated nothing. Anchor on the NO-OP REPLY instead: an
# Integer(0) is the executor deciding there is no work to do, and allocations below
# that point are the ones a no-op call still pays for.
EARLY_RETURN = re.compile(r"RespFrame::Integer\(0\)|Integer\(0\)")


def executors():
    """{fn_name: [(line_offset, text)]} for execute_plain_*_borrowed bodies."""
    src = open(FR, encoding="utf-8", errors="replace").read().splitlines()
    out, current, body = {}, None, []
    for line in src:
        m = re.match(r"    (?:pub )?fn (\w+)", line)
        if m:
            if current:
                out[current] = body
            current = m.group(1) if m.group(1).startswith("execute_plain_") else None
            body = []
        if current:
            body.append(line)
    if current:
        out[current] = body
    return out


def score(body):
    """(total alloc sites, sites appearing AFTER the first early return)."""
    total = after = 0
    seen_return = False
    for line in body:
        stripped = line.strip()
        if stripped.startswith("//"):
            continue
        hits = len(ALLOC.findall(stripped))
        total += hits
        if seen_return:
            after += hits
        if EARLY_RETURN.search(stripped):
            seen_return = True
    return total, after


def main():
    top = 15
    if "--top" in sys.argv:
        top = int(sys.argv[sys.argv.index("--top") + 1])

    rows = []
    for name, body in executors().items():
        total, after = score(body)
        if total:
            rows.append((after, total, name, len(body)))
    rows.sort(reverse=True)

    print("Borrowed executors ranked by allocation sites AFTER an early return --")
    print("the ones a no-op call still pays for.\n")
    print("%6s %6s  %-52s %s" % ("after", "total", "executor", "lines"))
    for after, total, name, n in rows[:top]:
        print("%6d %6d  %-52s %d" % (after, total, name, n))

    noop = sum(1 for r in rows if r[0])
    print("\n%d executors have at least one allocation site; %d have one that a"
          % (len(rows), noop))
    print("no-op call still pays. A count is not a cost -- rank here, then measure.")

    # frankenredis ALREADY ran the allocator campaign that frankenlibc and
    # frankentorch are now opening: gu5nf started at "58% of on-CPU samples were
    # allocator" and closed 35 sub-beads driving per-request allocation to 0.089
    # allocs/request at -P16. So the whole-program primitive is not the story here.
    #
    # What survived it is route-shaped, and that is exactly what a per-request
    # average hides: a handful of executors still allocate on a path that returns
    # a no-op reply. Baseline the residue so it cannot grow back quietly.
    if noop > NOOP_BASELINE:
        print("\nFAIL: %d executors allocate after a no-op reply, baseline %d. The "
              "gu5nf campaign drove per-request allocation to 0.089 at -P16; "
              "regrowing the residue is a decision, not a drift." % (noop, NOOP_BASELINE))
        return 1
    return 0


def self_test():
    """Pin the mechanism this screen was written from, by hand.

    execute_plain_move_borrowed must show allocation sites AFTER its early
    Integer(0) return -- that is the key.to_vec()/db_arg.to_vec() pair a missing
    key still pays for. If that stops being true the route was fixed and this
    anchor should be retired deliberately, not silently.
    """
    fns = executors()
    bad = []
    if "execute_plain_move_borrowed" not in fns:
        bad.append("execute_plain_move_borrowed not found; the parser missed it")
    else:
        total, after = score(fns["execute_plain_move_borrowed"])
        if total < 2:
            bad.append("MOVE shows %d allocation sites, expected at least 2 "
                       "(key.to_vec, db_arg.to_vec)" % total)
        if after < 2:
            bad.append("MOVE shows %d allocation sites after its early return, "
                       "expected at least 2 -- either the route was fixed or the "
                       "early-return detection broke" % after)

    # Mutation: a body with allocations but NO early return must score after=0,
    # or "after" means nothing and the ranking is just a total-count ranking.
    fake = ["    fn x() {", "        let a = k.to_vec();", "        a", "    }"]
    t, a = score(fake)
    if t < 1:
        bad.append("mutation setup failed: no allocation counted in the fake body")
    if a != 0:
        bad.append("VACUOUS: a body with no early return scored after=%d; the "
                   "after-return column is not actually conditional" % a)

    for line in bad:
        print("SELF-TEST FAIL: " + line)
    if bad:
        return 1
    print("self-test: MOVE shows allocations after its early return, and a body "
          "without an early return scores after=0")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else main())
