#!/usr/bin/env python3
"""keyspace_event_coverage_gate.py — every keyspace event the incumbent fires, fr must fire.

WHY A NAME CENSUS AND NOT A REFERENCE COUNT. `decorative_directive_gate.py` classifies config
DIRECTIVES and fails when one has no consumer. It passes while a single accepted VALUE of an
implemented directive does nothing -- which is what `notify-keyspace-events`' `m` flag did
(frankenredis-keymiss-oqhbi): accepted, echoed verbatim by CONFIG GET, and firing nothing.

The obvious extension is to count references to each `NOTIFY_*` constant. That does NOT work, and
it was tried before this file existed: when `keymiss` fired nothing, `NOTIFY_KEY_MISS` still had
eleven references -- the mask constants, the parser, the renderer, the tests. A constant is
mentioned everywhere; what goes missing is an EMISSION. So the unit that has to be censused is the
event NAME at its emit site, which is exactly how the bead found the defect by hand.

WHAT THIS CHECKS. Every `notifyKeyspaceEvent(NOTIFY_*, "<name>", ...)` literal in the vendored
7.2.4 source, against every `notify_keyspace_event(..., "<name>", ...)` in fr. 44 distinct names
upstream; the gate fails if any is absent from fr.

RUNS UNDER A BUILD FREEZE: no server, no cargo, no disk writes.

THE HONEST LIMIT. Finding the name at an emit site proves fr CAN fire it, not that it fires on the
same condition -- `keymiss` must fire on read misses and NOT on writes (upstream excludes
LOOKUP_WRITE), and this gate cannot see that. It closes the "absent entirely" hole, which is the
one that stayed open through a 44-event hand census and a passing directive gate.
"""
import glob
import io
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VENDORED = os.path.join(REPO, "legacy_redis_code", "redis", "src", "*.c")
FR_SOURCES = os.path.join(REPO, "crates", "*", "src", "*.rs")

# Events upstream fires that fr deliberately does not, each with the reason. Empty today, and it
# should stay that way: an entry here is a divergence, not a shrug. A name listed that fr DOES
# emit fails the gate, so the list cannot go stale silently.
ACCEPTED_ABSENT = {}
# `new` lived here until its emit site landed (fr-store, the single creation route into
# `entries`). The entry was removed in the same commit that implemented it, because this gate
# FAILS on a stale entry -- that is the whole point of the second check, and it is the first
# time it has been exercised.


def upstream_event_names():
    names = {}
    for path in sorted(glob.glob(VENDORED)):
        text = io.open(path, encoding="utf-8", errors="replace").read()
        for m in re.finditer(r'notifyKeyspaceEvent\s*\(\s*([A-Z_]+)\s*,\s*"([^"]+)"', text):
            names.setdefault(m.group(2), set()).add(os.path.basename(path))
    return names


def fr_emitted_names():
    """Names fr can put on the wire, from the TWO places they live.

    A literal census of `notify_keyspace_event(...)` alone reports 28 of 44 missing, and that is
    the census being wrong rather than fr: six emit sites pass the name as a VARIABLE, fed from
    `command_to_keyspace_event`, a verb-to-event table. Scanning only emit sites misses every
    name that table supplies; scanning the whole file for the quoted name instead reports 0
    missing, because unrelated literals -- a bench command list, a test expectation -- collide
    with event names. Both errors were made getting here; the fix is to read the two SPECIFIC
    places rather than a wider or narrower net.
    """
    emitted = set()
    for path in sorted(glob.glob(FR_SOURCES)):
        text = io.open(path, encoding="utf-8", errors="replace").read()
        for m in re.finditer(r'notify_keyspace_event\s*\([^;]*?"([^"]+)"', text, re.S):
            emitted.add(m.group(1))
        # the verb -> event-name table that feeds the variable-fed emit sites
        for fn in ("fn command_to_keyspace_event", "fn list_move_events",
                   "fn xgroup_keyspace_event"):
            start = text.find(fn)
            while start != -1:
                end = text.find("\n    }", start)
                if end == -1:
                    break
                for m in re.finditer(r'"([a-z][a-z0-9_.-]*)"', text[start:end]):
                    emitted.add(m.group(1))
                start = text.find(fn, end)
    return emitted


def main():
    upstream = upstream_event_names()
    emitted = fr_emitted_names()

    # (frankenredis-keymiss-oqhbi) A READER FLOOR, added because the mutation test that was
    # supposed to check the stale-entry path instead ran the gate from a temp directory: REPO
    # resolves from __file__, both globs matched nothing, and the gate PASSED on zero inputs.
    # A gate that is satisfied by reading nothing reports success loudest exactly when it has
    # been wired up wrong. 40 rather than 44 so a genuine upstream change does not wedge it.
    if len(upstream) < 40:
        print("FAIL — read only %d upstream events; expected >= 40. The vendored source at\n"
              "  %s\nwas not read. This is a wiring failure, not a parity result."
              % (len(upstream), VENDORED))
        return 1
    if not emitted:
        print("FAIL — read ZERO event names from fr at\n  %s\nSame wiring failure, other side."
              % FR_SOURCES)
        return 1

    missing = sorted(n for n in upstream if n not in emitted and n not in ACCEPTED_ABSENT)
    stale = sorted(n for n in ACCEPTED_ABSENT if n in emitted)

    print("upstream fires %d distinct keyspace events; fr emits %d names"
          % (len(upstream), len(emitted)))

    rc = 0
    if missing:
        print("\nFAIL — events the incumbent fires and fr never does:")
        for n in sorted(missing):
            print("    %-14s upstream: %s" % (n, ", ".join(sorted(upstream[n]))))
        print("  A subscriber to that channel waits forever, with no error anywhere.")
        rc = 1
    if stale:
        print("\nFAIL — ACCEPTED_ABSENT entries fr now emits (implemented; remove them):")
        for n in stale:
            print("    %s" % n)
        rc = 1
    if rc == 0:
        if ACCEPTED_ABSENT:
            print("\nPASS — with %d event(s) explicitly accounted for as ABSENT:"
                  % len(ACCEPTED_ABSENT))
            for n in sorted(ACCEPTED_ABSENT):
                print("    %s — %s" % (n, ACCEPTED_ABSENT[n]))
            print("  These are NOT emitted. A pass here means the gap is recorded, not closed.")
        else:
            print("\nPASS — every event the incumbent fires has an emit site in fr.")
        print("A PASS does not prove the CONDITION matches: this gate finds absent events,")
        print("not events fired on the wrong branch. See the module docstring.")
    return rc


if __name__ == "__main__":
    sys.exit(main())
