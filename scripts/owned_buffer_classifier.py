#!/usr/bin/env python3
"""Separate ARGV-ONLY owned buffers from LOAD-BEARING ones before hoisting any.

I wrote in aac0fb49c that the sibling fix was "mechanical in all of them". It is
not, and this exists so nobody acts on that sentence.

MOVE's `key_owned`/`db_owned` fed the metrics builder closure and NOTHING else, so
hoisting them into the closure removed pure waste.

I then "corrected" that to "keys_owned is LOAD-BEARING in all five siblings", which
was ALSO wrong. My slice ran from `fn X` to the next four-space `fn` and SPILLED
across function boundaries, so store.del lines belonging to LATER functions were
attributed to bitop. bitop's true body is 70 lines and carries no keys_owned at all.
Two wrong analyses of the same five executors, from one extraction bug, in opposite
directions -- which is why this classification lives in code with the boundary pinned
by a self-test instead of in a commit message.

What is true on the strict extractor: zinter carries one of each, and its keys_owned
IS load-bearing. bitop carries three argv-only buffers and nothing load-bearing. The
rest need reading individually rather than assuming a shared shape.

ARGV-ONLY: every use is an argv push/extend, or a .clone() feeding one. Allocate
eagerly, clone into argv, never touch again -- waste, because the closure that
consumes argv usually does not run (it fires only on a slowlog, latency-threshold or
time-budget breach).

SINGLE-USE argv-only buffers are the safest subset: one allocation and one clone, for
a closure that usually does not run. Those are the ones to hoist first.

  python3 scripts/owned_buffer_classifier.py [--self-test]
"""

import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FR = os.path.join(REPO, "crates/fr-runtime/src/lib.rs")


def executors():
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


def classify(body):
    """{name: (use_count, argv_only)} for every `let X_owned` in this body."""
    text = "\n".join(body)
    out = {}
    for name in sorted(set(re.findall(r"let (\w+_owned)", text))):
        uses = [l.strip() for l in body
                if name in l and ("let " + name) not in l]
        if not uses:
            continue
        argv_only = all(("argv" in u or "clone" in u) for u in uses)
        out[name] = (len(uses), argv_only)
    return out


def main():
    safest = []
    print("executor                                  argv-only            LOAD-BEARING")
    for name, body in sorted(executors().items()):
        got = classify(body)
        if not got:
            continue
        safe = [n for n, (u, a) in got.items() if a]
        held = [n for n, (u, a) in got.items() if not a]
        if not (safe and held):
            continue
        print("  %-38s %-20s %s"
              % (name.replace("execute_plain_", "").replace("_borrowed", ""),
                 ",".join(safe)[:20], ",".join(held)))
        for n, (u, a) in got.items():
            if a and u == 1:
                safest.append("%s::%s" % (name.replace("execute_plain_", ""), n))

    print("\nSAFEST SUBSET -- argv-only AND used exactly once (allocate, clone, done):")
    for s in safest:
        print("   %s" % s)
    print("\nHoist these first. Anything LOAD-BEARING must keep its owned buffer;")
    print("applying MOVE's transform there deletes work the command needs.")
    return 0


def self_test():
    """Pin the two facts a wrong extraction got wrong, in BOTH directions.

    This tool exists because I analysed the sibling executors twice and was wrong
    twice. First I called the fix "mechanical in all of them" (aac0fb49c). Then I
    "corrected" that to "keys_owned is LOAD-BEARING in all five" -- also wrong,
    because my slice ran from `fn X` to the next `    fn ` and SPILLED across
    function boundaries, so store.del lines from later functions were attributed to
    bitop. bitop's true body is 70 lines and contains no keys_owned at all.

    So the negative case is the boundary: bitop must NOT show keys_owned. And the
    positive case is that the classifier still discriminates, via zinter, which
    genuinely has both kinds.
    """
    fns = executors()
    bad = []

    # NEGATIVE: extraction must not spill. bitop's body carries exactly three
    # owned buffers and no keys_owned; seeing keys_owned here means the body
    # boundary broke again and every classification below it is suspect.
    if "execute_plain_bitop_borrowed" not in fns:
        bad.append("execute_plain_bitop_borrowed not found")
    else:
        got = classify(fns["execute_plain_bitop_borrowed"])
        if "keys_owned" in got:
            bad.append("bitop shows keys_owned; the body boundary SPILLED into a "
                       "later function, which is the exact error that produced two "
                       "wrong analyses of these executors")
        for want in ("op_owned", "dest_owned", "sources_owned"):
            if want not in got:
                bad.append("bitop should carry %s" % want)

    # POSITIVE: zinter genuinely has one of each, so a classifier returning a
    # constant answer fails here.
    if "execute_plain_zinter_borrowed" not in fns:
        bad.append("execute_plain_zinter_borrowed not found")
    else:
        got = classify(fns["execute_plain_zinter_borrowed"])
        if got.get("numkeys_owned", (0, False))[1] is not True:
            bad.append("zinter numkeys_owned should be ARGV-ONLY")
        if got.get("keys_owned", (0, True))[1] is not False:
            bad.append("zinter keys_owned should be LOAD-BEARING; classifying it as "
                       "waste would green-light deleting a buffer the op needs")

    for line in bad:
        print("SELF-TEST FAIL: " + line)
    if bad:
        return 1
    print("self-test: bitop does not spill (no keys_owned, three owned buffers), "
          "and zinter shows one ARGV-ONLY and one LOAD-BEARING")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else main())
