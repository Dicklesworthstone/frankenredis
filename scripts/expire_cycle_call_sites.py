#!/usr/bin/env python3
"""Track a structural divergence: fr runs the active-expire cycle PER COMMAND.

Upstream Redis 7.2.4 calls activeExpireCycle from exactly TWO places, both in
server.c -- ACTIVE_EXPIRE_CYCLE_SLOW from serverCron, and ACTIVE_EXPIRE_CYCLE_FAST
from beforeSleep. So it runs once per event-loop iteration, covering however many
commands that iteration drained.

frankenredis calls run_active_expire_cycle from ~150 distinct functions, nearly all
`execute_plain_*_borrowed` executors. So it runs once per COMMAND.

That difference is invisible on a workload with no TTLs, because frankenredis-bk7pi
added an early exit when count_expiring_keys() == 0 -- which is why exists_vs_redis,
seeding no TTLs at all, cannot see it. The moment ANY key carries a TTL the guard
stops firing, and under pipelining the ratio of cycles is the pipeline depth: at -P16
redis runs one fast cycle per loop while fr runs sixteen.

This does NOT claim the inline placement is wrong -- it may be deliberate, and a
per-command cycle has latency advantages a timer does not. It claims the divergence
is real, that its cost scales with pipeline depth on TTL-carrying workloads, and that
nothing currently measures it.

Exit 0 = counts unchanged. Exit 1 = the divergence grew or the upstream baseline
moved, either of which should be a decision rather than a drift.

  python3 scripts/expire_cycle_call_sites.py [--self-test]
"""

import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FR = os.path.join(REPO, "crates/fr-runtime/src/lib.rs")
UPSTREAM = os.path.join(REPO, "legacy_redis_code/redis/src")

# Baselines observed 2026-08-16. Deliberately exact: this file exists to make a
# CHANGE visible, and a range would hide exactly the drift it is watching for.
UPSTREAM_BASELINE = 2
FR_FN_BASELINE = 150


def upstream_call_sites():
    """Non-definition calls to activeExpireCycle in the vendored C source."""
    hits = []
    for name in sorted(os.listdir(UPSTREAM)):
        if not name.endswith(".c"):
            continue
        path = os.path.join(UPSTREAM, name)
        for n, line in enumerate(
                open(path, encoding="utf-8", errors="replace").read().splitlines(), 1):
            if "activeExpireCycle(" not in line:
                continue
            stripped = line.strip()
            # Skip the definition and comment lines; we want CALL sites.
            if stripped.startswith("void ") or stripped.startswith("*") \
                    or stripped.startswith("/*") or stripped.startswith("//"):
                continue
            hits.append("%s:%d" % (name, n))
    return hits


def fr_functions():
    """Distinct fr-runtime functions containing an inline cycle call."""
    src = open(FR, encoding="utf-8", errors="replace").read().splitlines()
    fns, current = set(), None
    for line in src:
        m = re.match(r"    (?:pub )?fn (\w+)", line)
        if m:
            current = m.group(1)
        if "run_active_expire_cycle(" in line and current:
            fns.add(current)
    return sorted(fns)


def main():
    up = upstream_call_sites()
    fr = fr_functions()
    print("upstream activeExpireCycle call sites: %d  %s" % (len(up), up))
    print("fr-runtime functions calling run_active_expire_cycle: %d" % len(fr))
    executors = [f for f in fr if f.startswith("execute_")]
    print("  of which command executors: %d" % len(executors))
    print("\nratio of cycles per pipelined batch at -P<N> is ~N:1 against upstream,")
    print("on any workload where at least one key carries a TTL (below that, the")
    print("bk7pi early exit makes it a counter read).")

    bad = []
    if len(up) != UPSTREAM_BASELINE:
        bad.append("upstream call sites %d, baseline %d -- the incumbent moved, so "
                   "the comparison itself needs revisiting" % (len(up), UPSTREAM_BASELINE))
    if len(fr) > FR_FN_BASELINE:
        bad.append("fr functions %d, baseline %d -- the divergence GREW; adding "
                   "another inline cycle should be a decision, not a drift"
                   % (len(fr), FR_FN_BASELINE))
    for line in bad:
        print("\nFAIL: " + line)
    return 1 if bad else 0


def self_test():
    """Pin the two facts that make this comparison meaningful.

    Hardcoded expectations, not values read back from the functions under test.
    """
    bad = []
    up = upstream_call_sites()
    if len(up) != 2:
        bad.append("expected exactly 2 upstream call sites, got %d: %s" % (len(up), up))
    if not all("server.c" in h for h in up):
        bad.append("upstream calls should all be in server.c (serverCron and "
                   "beforeSleep), got %s" % up)

    fr = fr_functions()
    if len(fr) < 100:
        bad.append("expected ~150 fr functions with an inline cycle, got %d -- if "
                   "this genuinely dropped, the divergence is closing and the "
                   "baseline should be lowered deliberately" % len(fr))
    if "execute_plain_move_borrowed" not in fr:
        bad.append("execute_plain_move_borrowed should contain an inline cycle; it "
                   "is the route that surfaced this")

    for line in bad:
        print("SELF-TEST FAIL: " + line)
    if bad:
        return 1
    print("self-test: upstream is 2 call sites, both in server.c; fr is %d functions "
          "including execute_plain_move_borrowed" % len(fr))
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else main())
