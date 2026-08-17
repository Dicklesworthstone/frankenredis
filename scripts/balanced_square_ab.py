#!/usr/bin/env python3
"""Balanced-square vs-incumbent A/B for FrankenRedis, usable on a CONTENDED host.

WHY THIS EXISTS
---------------
Every vs-incumbent harness in this repo gates on the host being quiet, and on a
64-way box shared by tens of agents that gate cannot be met. Measured here today:

  * `scripts/lua_eval_headtohead.sh` refused 21 of 21 invocations at an absolute
    loadavg ceiling; rescaling it per core (2b02caf16) helped, and it STILL
    refused 24 of 24 an hour later at load 27.9 against a 19.20 ceiling.
  * A four-arm throughput harness run at loadavg 58 produced A/A nulls of
    0.85-1.07 and a SAME-BINARY post/pre of 0.96-1.05 — two columns whose true
    value is exactly 1.0000. Recorded as INADMISSIBLE in
    `docs/perf_negative_evidence_ledger.md`.

The same wall was hit independently in franken_networkx, whose sanctioned
harness required five consecutive windows with EVERY cpu idle: its bead
`br-r37-c1-3s8x7` logged 25 consecutive attempts with zero admitted, and a run
aborted after 300 windows on one busy cpu. Its answer, committed as
`/data/projects/franken_networkx/scripts/balanced_square_ab.py` (72761094c), is
the design ported here. Three agents there hand-rolled it in scratchpads before
one committed it properly; this is a port of theirs, not a fourth hand-roll.

THE DESIGN. It does not try to make the host quiet. It makes the COMPARISON
immune to the host being busy:

  * Both arms run INSIDE one round, interleaved as a balanced square
    `A B B A A B B A`. Each arm occupies the same multiset of slot POSITIONS, so
    drift across a round — a peer's build starting, a cache warming, a governor
    step — hits both arms equally instead of biasing whichever went first.
  * Each arm carries its OWN A/A null: that arm's first-half slots divided by its
    second-half slots, which must come out 1.0. The square places the halves
    symmetrically, so a null that departs from 1.0 is drift or contention rather
    than slot position. Contention is therefore CAUGHT PER ROW, after the fact,
    instead of being excluded up front by a gate that can never pass.
  * A row whose null leaves [0.98, 1.02] is reported NULL-FAILED and its ratio is
    NOT a result. Refusing is the point.

This RELAXES NO EVIDENCE STANDARD. The incumbent is a live vendored
`redis-server` started in this same invocation; every arm's ELF SHA-256 is read
from `/proc/<pid>/exe` of the already-running process, so the harness cannot
compare a build against itself by accident; and provenance carries the OBSERVED
thread count, host, governor and runtime ISA. It replaces an unsatisfiable
precondition with a sound experimental design, nothing more.

USAGE
-----
    scripts/balanced_square_ab.py --fr-bin /tmp/fr_head --shapes cascade
    scripts/balanced_square_ab.py --fr-bin /tmp/fr_head --shapes eval --rounds 15

    --shapes     a registered shape set (see --list)
    --rounds     balanced squares per row (default 9)
    --ops        redis-benchmark operations per timed slot (default 50000)
    --pipeline   redis-benchmark -P depth (default 16)
    --expect-elf first 16 hex chars of the fr ELF you INTEND to measure; the run
                 aborts on mismatch, because pointing at a stale /tmp copy is the
                 cheapest way to publish a number about the wrong binary.

Ratio convention is fr_ops_per_sec / redis_ops_per_sec, so > 1 means FrankenRedis
is faster. That is the convention the ledger rows use.

ADDING A SHAPE SET. Append to SHAPE_SETS. A shape is
`(label, [seed commands], [benchmark argv])`. Every shape is error-probed on BOTH
engines before timing, because `redis-benchmark` counts an error reply as a
completed request and a refused command otherwise reads as enormous throughput.
Include at least one row the change under test CANNOT affect, as a control.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import platform
import random
import re
import shutil
import socket
import tempfile
import statistics
import subprocess
import sys
import time

SQUARE = "ABBAABBA"
NULL_BOUND = 0.02



# (frankenredis-eh2ct) Commands whose cost SCALES with the collection they touch.
# A shape running one of these over a 3-element collection measures the fixed
# per-command cost, not the command — see `audit_shape_sizes`.
SIZE_SCALING_COMMANDS = frozenset({
    "SORT", "SORT_RO", "LRANGE", "SMEMBERS", "ZRANGE", "ZRANGEBYSCORE",
    "ZRANGEBYLEX", "ZREVRANGE", "ZREVRANGEBYSCORE", "ZREVRANGEBYLEX", "HGETALL",
    "HKEYS", "HVALS", "SINTER", "SUNION", "SDIFF", "ZDIFF", "ZINTER", "ZUNION",
    "SINTERCARD", "ZINTERCARD", "MGET", "HMGET", "ZMSCORE", "SMISMEMBER",
    "XRANGE", "XREVRANGE", "BITCOUNT", "LPOS", "SRANDMEMBER", "ZRANDMEMBER",
    "HRANDFIELD", "GEOSEARCH", "PFCOUNT", "LCS", "KEYS", "SCAN",
})


def seeded_collection_size(seeds):
    """Largest scaling input the seeds build, as (size, unit).

    (frankenredis-eh2ct) THE FIRST VERSION OF THIS WAS WRONG IN THREE WAYS, and the
    third one indicted my own fix:

      * it took a MAX over individual seed commands instead of ACCUMULATING repeated
        adds to the same key, so `xrange_2` (two XADDs) reported n=1;
      * it did not know GEOADD, so `geosearch` (two GEOADDs) reported n=0;
      * and therefore `geosearch_64` — the 64-member sibling I added specifically to
        fix an intercept row — ALSO reported n=0 and would have been flagged as
        degenerate by my own audit.

    It also had no unit: BITCOUNT over `SET bb abcdefghijklmnop` scales with STRING
    LENGTH, not with a collection, and reporting that as "0" implied no input at all
    when the real answer is 16 bytes. A 16-byte BITCOUNT is still a fixed-cost row, so
    flagging it was right for the wrong reason — and a detector that is right for the
    wrong reason will be wrong when the shape changes.

    Counts per KEY and returns the largest, so two seeds feeding one key add up while
    two seeds feeding different keys do not.
    """
    per_key = {}
    unit_of = {}

    def bump(key, n, unit):
        per_key[key] = per_key.get(key, 0) + n
        unit_of[key] = unit

    for seed in seeds:
        tok = seed.split()
        if len(tok) < 2:
            continue
        cmd, key = tok[0].upper(), tok[1]
        rest = len(tok) - 2
        if cmd in ("RPUSH", "LPUSH", "SADD", "PFADD"):
            bump(key, rest, "elements")
        elif cmd == "ZADD":
            bump(key, rest // 2, "elements")
        elif cmd in ("HSET", "HMSET"):
            bump(key, rest // 2, "fields")
        elif cmd == "MSET":
            # MSET has no single key: every pair is its own key, and a shape reading
            # N of them (MGET a b c) scales with how many were seeded.
            bump("<mset>", (len(tok) - 1) // 2, "keys")
        elif cmd == "GEOADD":
            bump(key, rest // 3, "members")
        elif cmd == "XADD":
            bump(key, 1, "entries")
        elif cmd in ("SET", "SETRANGE"):
            # Scaling unit is the VALUE's byte length, not a count.
            value = tok[-1]
            per_key[key] = max(per_key.get(key, 0), len(value))
            unit_of[key] = "bytes"
        elif cmd == "APPEND":
            bump(key, len(tok[-1]), "bytes")
    if not per_key:
        return 0, "unknown"
    key = max(per_key, key=lambda k: per_key[k])
    return per_key[key], unit_of.get(key, "elements")


def _selftest_sizes():
    """Pin the size parser against HARDCODED expectations.

    Deliberately not derived from SHAPE_SETS: an oracle read out of the thing under
    test proves only that the code agrees with itself. Every expectation below was
    counted by hand from the seed string.
    """
    cases = [
        (["RPUSH sl c a b"], 3, "elements"),
        (["RPUSH sl64 " + " ".join(f"w{i}" for i in range(64))], 64, "elements"),
        (["SADD s1 m1 m2 m3", "SADD s2 m2 m3 m4"], 3, "elements"),
        # Two adds to the SAME key must ACCUMULATE (the max-based bug).
        (["SADD s1 m1 m2", "SADD s1 m3"], 3, "elements"),
        (["ZADD zd1 1 a 2 b 3 c"], 3, "elements"),
        (["HSET h f1 v1 f2 v2 f3 v3"], 3, "fields"),
        # Two XADDs to one stream are TWO entries, not one.
        (["XADD xst 1-1 f v", "XADD xst 1-2 f v"], 2, "entries"),
        # GEOADD triples, single and repeated.
        (["GEOADD g 13.36 38.11 P1", "GEOADD g 15.08 37.50 P2"], 2, "members"),
        (["GEOADD g64 " + " ".join(f"13.{i} 37.{i} M{i}" for i in range(64))],
         64, "members"),
        (["MSET a 1 b 2 c 3"], 3, "keys"),
        # A string shape's unit is BYTES, and 16 is not "no input".
        (["SET bb abcdefghijklmnop"], 16, "bytes"),
        ([], 0, "unknown"),
    ]
    bad = 0
    print("%-46s %-8s %-10s %s" % ("seeds", "size", "unit", "expected"))
    for seeds, want_n, want_unit in cases:
        got_n, got_unit = seeded_collection_size(seeds)
        ok = (got_n, got_unit) == (want_n, want_unit)
        bad += 0 if ok else 1
        shown = (seeds[0][:44] + "..") if seeds else "(none)"
        print("%-46s %-8s %-10s %s %s"
              % (shown, got_n, got_unit, f"{want_n} {want_unit}",
                 "ok" if ok else "FAIL"))
    print("size selftest: %d case(s) failed" % bad)
    bad += _selftest_normalised_bounds()
    return 1 if bad else 0


def _selftest_normalised_bounds() -> int:
    """The bounds rule, pinned on the row that motivated it.

    Case 1 is `geosearch_64` run 4, numbers copied from the banked measurement. Its POINT
    estimate is 1.0044 — above 1.0, and it was banked elsewhere at 1.0094 and described as
    "an inversion, fr AHEAD". Its own bounds run 0.9857-1.0359, so it is a STRADDLES-1 and
    no crossing may be claimed. If this ever returns AHEAD, the harness has gone back to
    quoting point estimates and the thing that produced a phantom inversion is back.
    """
    cases = [
        # label, row ratio/ci, control ratio/ci, want verdict, want wider-normaliser
        ("geosearch_64 run4", 1.1119, (1.1065, 1.1297), 1.1071, (1.0906, 1.1225),
         "STRADDLES-1", True),
        ("clear ahead", 1.3000, (1.2800, 1.3200), 1.1000, (1.0900, 1.1100),
         "AHEAD", False),
        ("clear behind", 0.9000, (0.8900, 0.9100), 1.1000, (1.0900, 1.1100),
         "BEHIND", False),
    ]
    failures = 0
    print("%-22s %8s %9s %9s  %-12s %s"
          % ("normalised-bounds case", "point", "worst", "best", "verdict", "check"))
    for label, r, rci, c, cci, want_verdict, want_wider in cases:
        point, worst, best, verdict = normalised_bounds(r, rci, c, cci)
        wider = normaliser_is_wider(r, rci, c, cci)
        ok = verdict == want_verdict and wider == want_wider
        failures += 0 if ok else 1
        print("%-22s %8.4f %9.4f %9.4f  %-12s %s"
              % (label, point, worst, best, verdict,
                 "ok" if ok else "FAIL (wanted %s, wider=%s)" % (want_verdict, want_wider)))
    # The defect itself: a point estimate ABOVE 1.0 whose worst bound is BELOW it. A rule
    # that only reported the point would have called case 1 a crossing.
    point, worst, _best, _v = normalised_bounds(1.1119, (1.1065, 1.1297),
                                                1.1071, (1.0906, 1.1225))
    if not (point > 1.0 and worst < 1.0):
        failures += 1
        print("  %-20s FAIL: fixture no longer reproduces point>1 with worst<1"
              % "bounds: defect shown")
    else:
        print("  %-20s point %.4f > 1.0 but worst %.4f < 1.0  ok"
              % ("bounds: defect shown", point, worst))
    print("normalised-bounds selftest: %d case(s) failed" % failures)
    return failures


# (frankenredis-eh2ct) Per-UNIT minimums. Once the parser started reporting units it
# became obvious that one threshold cannot serve them: `bitcount` seeds a 16-BYTE
# string, and 16 compared against an element threshold of 4 reads as "big enough"
# when 16 bytes is two words of work — still a fixed-cost row. Bytes need a much
# higher bar than elements before per-unit cost can dominate the fixed cost.
MIN_SCALING_INPUT = {
    "elements": 4,
    "fields": 4,
    "members": 4,
    "entries": 4,
    "keys": 4,
    "bytes": 64,
    "unknown": 4,
}


def audit_shape_sizes(min_elements=None):
    """Flag registered shapes that measure an INTERCEPT and call it a command.

    (frankenredis-eh2ct) This exists because of a measured inversion, not a theory.
    `sort_ro_alpha` seeds a THREE-element list, and a 3-element sort does ~3
    comparisons — so per-element and per-comparison cost are the same number and the
    fixed per-command cost dominates the whole row. On that shape fr read 0.8118 and
    SORT stood as "fr's worst route" for weeks. Adding a 64-element point put fr
    AHEAD at 1.1097 (1.0195 control-normalised), and the n=3 row turned out to be
    INADMISSIBLE in the same run — `null_redis` 1.0201, past the 0.02 bound, because
    at three elements the INCUMBENT's own A/A null fails.

    So a small shape is not automatically wrong; it is wrong when its row is read as
    characterising the COMMAND. This flags the candidates; only SORT has been shown
    to invert, and the rest are UNTESTED rather than known-bad.

    Deletion condition: remove this when every scaling command on the board carries a
    second, larger-N point, since then the crossover is always visible and there is
    nothing left to warn about.
    """
    flagged = []
    total = 0
    for set_name, shapes in SHAPE_SETS.items():
        for label, seeds, argv in shapes:
            if not argv or argv[0].upper() not in SIZE_SCALING_COMMANDS:
                continue
            total += 1
            n, unit = seeded_collection_size(seeds)
            floor = min_elements or MIN_SCALING_INPUT.get(unit, 4)
            if n < floor:
                flagged.append((n, set_name, label, argv[0].upper(), unit))
    flagged.sort()
    print("size-scaling shapes registered: %d" % total)
    print("seeded below their unit's floor %s: %d\n"
          % (MIN_SCALING_INPUT if min_elements is None else min_elements, len(flagged)))
    print("%-6s %-10s %-12s %-22s %s" % ("size", "unit", "set", "shape", "command"))
    for n, set_name, label, cmd, unit in flagged:
        print("%-6s %-10s %-12s %-22s %s" % (n, unit, set_name, label, cmd))
    paired = {
        label
        for _shapes in SHAPE_SETS.values()
        for label, _s, _a in _shapes
        if any(label.endswith(suf) for suf in ("_64", "_big", "_large"))
    }
    print("\nshapes that DO carry a larger-N sibling: %s"
          % (", ".join(sorted(paired)) if paired else "(none)"))
    print(
        "\nA flagged row is not wrong — it is an INTERCEPT row. Quote it as fixed\n"
        "per-command cost, or register a larger-N sibling before quoting it as the\n"
        "command's ratio. SORT is the one case measured both ways: 0.8118 at n=3\n"
        "(inadmissible) versus 1.1097 at n=64 (admissible)."
    )
    return 0

def classify_row(ratio, ci_low, ci_high, null_redis, null_fr, null_bound=NULL_BOUND):
    """Decide a row's verdict. PURE — no sockets, no timing, so it is unit-testable.

    (frankenredis-enrhw) This used to be three inline branches, and the middle one
    was wrong in a way that inline code hid: ADMISSIBLE required only that the
    ratio's bootstrap CI exclude 1.0, and never that the effect exceed the null bias
    the same row had just been FORGIVEN. So a row whose own instrument was measured
    1.66% off (null_fr = 1.0166, admitted because 0.0166 <= 0.02) could be certified
    on a ratio of 1.008 — an effect HALF the size of the bias in the instrument that
    produced it. Observed nulls in this harness run 0.9928-1.0166, so the tolerated
    bias reaches 1.7% while the old decision threshold was effectively zero.

    The rule now: an effect must clear the LARGER of the two nulls' observed bias
    before it counts. NULL_BOUND becomes the FLOOR ON WHAT MAY BE CLAIMED rather
    than only a pass/fail filter. That is strictly stricter — it can refuse rows the
    old rule admitted and can never admit one it refused — which is the safe
    direction for a gate four panes run and I cannot exercise end to end.

    Returns (verdict, binding_term) so a row can say WHICH constraint decided it;
    a gate that cannot name its binding constraint gets re-litigated forever.
    """
    bias = max(abs(null_redis - 1.0), abs(null_fr - 1.0))
    if abs(null_redis - 1.0) > null_bound or abs(null_fr - 1.0) > null_bound:
        return "NULL-FAILED", "null_bound"
    if ci_low <= 1.0 <= ci_high:
        return "STRADDLES-1", "ci_brackets_1"
    if abs(ratio - 1.0) <= bias:
        # The effect is inside the instrument's own demonstrated error. Not a null
        # failure (the nulls passed) and not a straddle (the CI is clean) — a third
        # outcome the old three-branch rule had no name for, which is exactly why it
        # returned ADMISSIBLE here.
        return "UNDER-NULL", "effect_le_null_bias"
    return "ADMISSIBLE", "effect_gt_null_bias"


def _selftest_classify():
    """Exercise the decision rule directly. No server, no timing, no build."""
    B = NULL_BOUND
    cases = [
        # (name, ratio, ci_low, ci_high, n_redis, n_fr, expect)
        # THE REGRESSION THIS FIXES: a real observed null with a small effect.
        ("observed_null_small_effect", 1.008, 1.004, 1.012, 1.0000, 1.0166, "UNDER-NULL"),
        # Same nulls, effect comfortably past the bias -> still admissible.
        ("observed_null_big_effect", 1.250, 1.200, 1.300, 1.0000, 1.0166, "ADMISSIBLE"),
        # Clean nulls, modest effect -> admissible (must not over-reject).
        ("clean_nulls_modest_effect", 1.030, 1.020, 1.040, 1.0010, 0.9995, "ADMISSIBLE"),
        # A null past the bound still fails first, whatever the effect.
        ("null_over_bound", 2.000, 1.900, 2.100, 1.0500, 1.0000, "NULL-FAILED"),
        # A straddling CI outranks the bias test.
        ("straddles", 1.000, 0.980, 1.020, 1.0000, 1.0000, "STRADDLES-1"),
        # BELOW 1.0 (fr slower) must be judged on MAGNITUDE, not sign — a gate that
        # only guarded the fast side would be arm-asymmetric, the defect three
        # projects hit today.
        ("slow_side_under_null", 0.992, 0.988, 0.996, 1.0000, 1.0166, "UNDER-NULL"),
        ("slow_side_real", 0.750, 0.700, 0.800, 1.0000, 1.0166, "ADMISSIBLE"),
        # Exactly at the bias is NOT a claim (boundary is inclusive against us).
        ("exactly_at_bias", 1.0166, 1.010, 1.020, 1.0000, 1.0166, "UNDER-NULL"),
    ]
    bad = 0
    print("%-28s %-8s %-9s %-12s %s" % ("case", "ratio", "bias", "got", "expect"))
    for name, ratio, lo, hi, nr, nf, expect in cases:
        got, binding = classify_row(ratio, lo, hi, nr, nf, B)
        bias = max(abs(nr - 1.0), abs(nf - 1.0))
        ok = got == expect
        bad += 0 if ok else 1
        print("%-28s %-8.4f %-9.4f %-12s %-12s %s (%s)"
              % (name, ratio, bias, got, expect, "ok" if ok else "FAIL", binding))
    # A stricter rule must never ADMIT something the old rule refused. The old rule
    # was: nulls_ok and not straddle -> ADMISSIBLE. So every ADMISSIBLE here must
    # also have satisfied the old rule; assert that direction explicitly.
    for name, ratio, lo, hi, nr, nf, _e in cases:
        got, _b = classify_row(ratio, lo, hi, nr, nf, B)
        old_admissible = (
            abs(nr - 1.0) <= B and abs(nf - 1.0) <= B and not (lo <= 1.0 <= hi)
        )
        if got == "ADMISSIBLE" and not old_admissible:
            print("MONOTONICITY VIOLATED on %s: new rule admits what old refused" % name)
            bad += 1
    print("selftest: %d case(s) failed" % bad)
    return 1 if bad else 0

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-benchmark")
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
CLI = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-cli")

# Shapes are grouped so a row set can be named on the command line rather than
# re-typed. The trailing control in each set is a command the work under test
# does not touch; a control that moves with the candidate means the row set is
# measuring the harness, not the change.
SHAPE_SETS: dict[str, list[tuple[str, list[str], list[str]]]] = {
    # The nine shapes front-classified onto the dispatch floor (frankenredis-ozrro).
    "cascade": [
        ("sintercard", ["SADD sc:a m1 m2 m3", "SADD sc:b m2 m3 m4"],
         ["SINTERCARD", "2", "sc:a", "sc:b"]),
        # (frankenredis-mnzgy) The next three do NOT meet the admission bar
        # this file states, and scripts/shape_admission_probe.py flags them. They
        # are annotated rather than removed, because deleting another agent's
        # registered shapes is not this audit's call -- but do not read a ratio
        # off them without reading this first.
        #
        # zrandmember/srandmember are RANDOM: the reply differs run to run and
        # between engines, and its LENGTH differs too ("m9" is 2 bytes, "m10" is
        # 3), so the two arms do not write identical byte counts. The per-call
        # work is comparable, so the rows are indicative, not byte-exact.
        ("zrandmember", ["ZADD zz 1 a 2 b 3 c 4 d"], ["ZRANDMEMBER", "zz", "2"]),
        ("srandmember", ["SADD sbig m1 m2 m3 m4 m5 m6 m7 m8 m9 m10"],
         ["SRANDMEMBER", "sbig", "2"]),
        # COPY without REPLACE returned 1 on the FIRST call and 0 on every call
        # after, so 19,999 of 20,000 ops measured the destination-exists early
        # return rather than a copy -- the row was named "copy" and measured a
        # no-op. REPLACE makes every op perform the copy and return 1, which is
        # both what the name promises and stable under repetition.
        ("copy", ["SET kk vvvvvvvvvvvvvvvv"], ["COPY", "kk", "kdst", "REPLACE"]),
        # pttl's VALUE drifts (it returns remaining ms), but the digit count -- and
        # so the reply byte length and the work done -- is constant at this TTL
        # magnitude: 900000000 loses ~100ms over a 20k-op run and stays 9 digits.
        # A SMALLER TTL here would change reply length mid-run and break the row.
        ("pttl", ["SET bb abcdefghijklmnop", "PEXPIRE bb 900000000"], ["PTTL", "bb"]),
        ("expiretime", ["SET kk vvvvvvvvvvvvvvvv", "EXPIREAT kk 4102444800"],
         ["EXPIRETIME", "kk"]),
        ("publish", [], ["PUBLISH", "ch", "hello"]),
        ("getbit", ["SET bb abcdefghijklmnop"], ["GETBIT", "bb", "5"]),
        ("geohash", ["GEOADD gg 13.361389 38.115556 Palermo"], ["GEOHASH", "gg", "Palermo"]),
        # Control: GET is not front-classified by that work.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # (frankenredis-bcva8/t7qgs/in98j/vlrnn/bj3mq/fhjnd) The zset and scan READ
    # routes that were front-classified onto the dispatch floor and shipped on
    # instruction counts alone. Every shape here is READ-ONLY on purpose: a
    # mutating shape like ZREMRANGEBYLEX or LPOP COUNT drains or empties its key
    # within the first few of redis-benchmark's requests and then measures the
    # absent/empty path for the remaining tens of thousands, which is a steady
    # state neither route was shipped for. Those need a harness that restores
    # state per request and are deliberately NOT faked in here.
    "zsetreads": [
        ("zrevrange", ["ZADD zr 1 a 2 b 3 c 4 d 5 e"], ["ZREVRANGE", "zr", "0", "-1"]),
        ("zrangebyscore", ["ZADD zr 1 a 2 b 3 c 4 d 5 e"],
         ["ZRANGEBYSCORE", "zr", "2", "4"]),
        ("zrevrangebyscore", ["ZADD zr 1 a 2 b 3 c 4 d 5 e"],
         ["ZREVRANGEBYSCORE", "zr", "4", "2"]),
        ("zrevrangebylex", ["ZADD zl 0 a 0 b 0 c 0 d 0 e"],
         ["ZREVRANGEBYLEX", "zl", "[e", "[b"]),
        ("zdiff", ["ZADD zd1 1 a 2 b 3 c", "ZADD zd2 1 b"], ["ZDIFF", "2", "zd1", "zd2"]),
        ("zinter", ["ZADD zd1 1 a 2 b 3 c", "ZADD zd2 1 b"], ["ZINTER", "2", "zd1", "zd2"]),
        ("sscan0", ["SADD ss m1 m2 m3 m4 m5 m6 m7 m8"], ["SSCAN", "ss", "0"]),
        ("hscan0", ["HSET hh f1 v1 f2 v2 f3 v3 f4 v4"], ["HSCAN", "hh", "0"]),
        ("zscan0", ["ZADD zr 1 a 2 b 3 c 4 d 5 e"], ["ZSCAN", "zr", "0"]),
        # Control: GET is not front-classified by that work.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # The standing Lua target: 50 redis.call('GET') per EVAL.
    "eval": [
        ("eval_50x_get", ["SET k val"],
         ["EVAL", "for i=1,50 do redis.call('GET', KEYS[1]) end return 1", "1", "k"]),
        ("get_control", ["SET k val"], ["GET", "k"]),
    ],
    # Commands that MUTATE their key, measured on their NO-OP path so the square is
    # valid at all. (frankenredis-va5me, frankenredis-5yhyh, frankenredis-wgrny)
    #
    # These three beads were recorded as unmeasurable here, correctly: redis-benchmark
    # fires tens of thousands of identical requests, so a real ZREMRANGEBYRANK /
    # ZREMRANGEBYLEX / LPOP COUNT drains its key within the first few and every
    # remaining request measures the EMPTY case. The ratio you get is then a fiction
    # about a command that stopped running.
    #
    # A no-op shape removes the problem rather than working around it: request 1 and
    # request 50,000 do exactly the same work, so the square measures one steady
    # thing. Each shape below was probed on BOTH engines before being added here —
    # identical non-error reply, and the collection size unchanged after 200
    # repetitions (zr/zl stay at 3, nosuchlist stays absent).
    #
    # This is the DISPATCH-path cost of these commands, which is what the front
    # classification work actually changed; it is NOT a claim about the cost of
    # removing elements, and no row from this set may be quoted as one.
    "mutnoop": [
        # start > stop: an empty rank range, so nothing is removed and 0 comes back.
        ("zremrangebyrank_noop", ["ZADD zr 1 a 2 b 3 c"],
         ["ZREMRANGEBYRANK", "zr", "5", "4"]),
        # min > max lexicographically: an empty lex range, same reasoning.
        ("zremrangebylex_noop", ["ZADD zl 0 a 0 b 0 c"],
         ["ZREMRANGEBYLEX", "zl", "[x", "[a"]),
        # Missing key: the COUNT form returns a null array and creates nothing.
        ("lpop_count_missing", [], ["LPOP", "nosuchlist", "10"]),
        ("rpop_count_missing", [], ["RPOP", "nosuchlist", "10"]),
        # Control: GET is untouched by the dispatch work these rows are about, and
        # without it none of the rows above can be normalised.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # Multi-key reads and *STORE writes. (frankenredis-3nn63, frankenredis-gdnqr,
    # frankenredis-fc7w0, frankenredis-uld9l, frankenredis-9601c, frankenredis-8t4uu,
    # frankenredis-ox2xq)
    #
    # The *STORE commands WRITE, but they are safe to hammer because they are
    # IDEMPOTENT: the destination is recomputed from unchanging sources, so request
    # 50,000 produces exactly what request 1 did. That is a different property from
    # the `mutnoop` set above, where the effect ACCUMULATED and the command had to be
    # reduced to a no-op. ZMPOP genuinely pops, so it is measured on a missing key.
    #
    # Every shape here was probed on BOTH engines before registration: identical
    # non-error reply, and the reply UNCHANGED after 200 repetitions.
    # (frankenredis-hxgsz) Routes NO existing set covers. The four sets
    # above are almost entirely reads on keys with no TTL, so whole families --
    # the write path, the container-length reads, the key-metadata reads -- have
    # never been measured against the incumbent at all. Every shape here cleared
    # the same admission bar the others did, probed on BOTH engines before
    # registration: identical non-error reply, and the reply UNCHANGED after 200
    # repetitions (scratchpad/shape_admit_probe.py, 20 admitted, 0 rejected).
    # setex_same is deliberately included: it is the only write here that leaves a
    # TTL behind, and fr's per-command expire cycle makes that a different
    # workload (frankenredis-kiyxn).
    "unswept": [
        ("strlen", ["SET s abcdefghijklmnop"], ["STRLEN", "s"]),
        ("getrange", ["SET s abcdefghijklmnop"], ["GETRANGE", "s", "2", "9"]),
        ("llen", ["RPUSH l a b c d e"], ["LLEN", "l"]),
        ("lrange_5", ["RPUSH l a b c d e"], ["LRANGE", "l", "0", "-1"]),
        ("hlen", ["HSET h f1 v1 f2 v2 f3 v3"], ["HLEN", "h"]),
        ("hget", ["HSET h f1 v1 f2 v2 f3 v3"], ["HGET", "h", "f2"]),
        ("scard", ["SADD st m1 m2 m3 m4 m5"], ["SCARD", "st"]),
        ("zcard", ["ZADD z 1 a 2 b 3 c"], ["ZCARD", "z"]),
        ("type", ["SET s abcdefghijklmnop"], ["TYPE", "s"]),
        ("object_encoding", ["SET s abcdefghijklmnop"], ["OBJECT", "ENCODING", "s"]),
        ("ttl_nonvolatile", ["SET s abcdefghijklmnop"], ["TTL", "s"]),
        ("persist_noop", ["SET s abcdefghijklmnop"], ["PERSIST", "s"]),
        ("set_same", [], ["SET", "wk", "vvvvvvvvvvvvvvvv"]),
        ("setex_same", [], ["SETEX", "wx", "100", "vvvvvvvvvvvvvvvv"]),
        ("setrange_same", ["SET sr abcdefghijklmnop"], ["SETRANGE", "sr", "3", "xy"]),
        ("hset_same", ["HSET h f1 v1"], ["HSET", "h", "f1", "v1"]),
        ("sadd_same", ["SADD st m1"], ["SADD", "st", "m1"]),
        ("zadd_same", ["ZADD z 1 a"], ["ZADD", "z", "1", "a"]),
        ("getex_persist", ["SET gx abcdefghijklmnop"], ["GETEX", "gx", "PERSIST"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # (frankenredis-mnzgy) Second unswept batch: the TTL-WRITE family plus the
    # remaining O(1) metadata reads. PERSIST and SETEX came out worst of anything
    # measured (frankenredis-59wjs, 1.7732x instructions to do nothing), so the
    # commands that SET a TTL are the obvious next place to look. All cleared
    # scripts/shape_admission_probe.py on both engines: 15 admitted, 0 rejected.
    "unswept2": [
        ("del_missing", [], ["DEL", "nosuchkey"]),
        ("unlink_missing", [], ["UNLINK", "nosuchkey"]),
        ("expire_same", ["SET s abcdefghijklmnop"], ["EXPIRE", "s", "10000"]),
        ("pexpire_same", ["SET s abcdefghijklmnop"], ["PEXPIRE", "s", "10000000"]),
        ("expireat_same", ["SET s abcdefghijklmnop"], ["EXPIREAT", "s", "4102444800"]),
        ("lpos", ["RPUSH l a b c d e"], ["LPOS", "l", "c"]),
        ("object_refcount", ["SET s abcdefghijklmnop"], ["OBJECT", "REFCOUNT", "s"]),
        ("memory_usage", ["SET s abcdefghijklmnop"], ["MEMORY", "USAGE", "s"]),
        ("hexists", ["HSET h f1 v1"], ["HEXISTS", "h", "f1"]),
        ("sismember", ["SADD st m1 m2 m3"], ["SISMEMBER", "st", "m2"]),
        ("zscore", ["ZADD z 1 a 2 b"], ["ZSCORE", "z", "b"]),
        ("zrank", ["ZADD z 1 a 2 b"], ["ZRANK", "z", "b"]),
        ("hstrlen", ["HSET h f1 v1"], ["HSTRLEN", "h", "f1"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # (frankenredis-f9zmz) Third unswept batch. The no-op / MISS family has been
    # the richest vein measured so far -- PERSIST on a non-volatile key 1.7732x,
    # UNLINK on a missing key 2.1708x -- so this leans into misses across every
    # type, plus writes whose reply is stable under repetition. All 19 cleared
    # scripts/shape_admission_probe.py on both engines, 0 rejected.
    "unswept3": [
        ("hdel_missing", ["HSET h f1 v1"], ["HDEL", "h", "nofield"]),
        ("srem_missing", ["SADD st m1"], ["SREM", "st", "nomember"]),
        ("zrem_missing", ["ZADD z 1 a"], ["ZREM", "z", "nomember"]),
        ("lrem_missing", ["RPUSH l a b c"], ["LREM", "l", "0", "nosuch"]),
        ("exists_missing", [], ["EXISTS", "nosuchkey"]),
        ("touch_missing", [], ["TOUCH", "nosuchkey"]),
        ("type_missing", [], ["TYPE", "nosuchkey"]),
        ("get_missing", [], ["GET", "nosuchkey"]),
        ("setnx_existing", ["SET nx v"], ["SETNX", "nx", "other"]),
        ("lset_same", ["RPUSH l a b c"], ["LSET", "l", "0", "a"]),
        ("setbit_same", ["SET bb abcdefghijklmnop"], ["SETBIT", "bb", "5", "0"]),
        ("getset_same", ["SET gs vvvvvvvvvvvvvvvv"], ["GETSET", "gs", "vvvvvvvvvvvvvvvv"]),
        ("bitcount", ["SET bb abcdefghijklmnop"], ["BITCOUNT", "bb"]),
        ("bitpos", ["SET bb abcdefghijklmnop"], ["BITPOS", "bb", "1"]),
        ("dbsize", ["SET s v"], ["DBSIZE"]),
        ("smembers", ["SADD st m1 m2 m3 m4 m5"], ["SMEMBERS", "st"]),
        ("hgetall", ["HSET h f1 v1 f2 v2 f3 v3"], ["HGETALL", "h"]),
        ("lindex", ["RPUSH l a b c d e"], ["LINDEX", "l", "2"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # (frankenredis-9tni0) Fourth unswept batch: families NO sweep had touched --
    # streams, geo, HyperLogLog, SORT, set-cardinality intersections. All 15
    # cleared scripts/shape_admission_probe.py on both engines, 0 rejected.
    # (frankenredis-r9mqp) TWO POINTS ON ONE COMMAND, kept as its own set so the
    # crossover can be re-checked in a few minutes rather than by running a 16-shape
    # sweep. The n=3 and n=64 SORT rows plus the control: that is the whole question,
    # and a set this small fits comfortably inside a single certification window,
    # which is why the 16-shape version of this run got killed at the tool timeout.
    # (frankenredis-eh2ct) THREE SMALL/LARGE PAIRS IN ONE INVOCATION, to test whether
    # the intercept problem the SORT row exposed generalises. The audit flagged 21
    # size-scaling shapes seeded with <=3 elements, but only SORT had been measured
    # both ways, so the claim "the board misdirects lever selection" rested on a
    # single case. These are the three flagged shapes where fr measured BEHIND at
    # tiny n in the unswept4 sweep (xrange_2 0.9935, geosearch 0.9841) plus hgetall,
    # which is the most-quoted small-collection read — an inversion is only possible
    # where fr currently trails, so those are the informative candidates.
    #
    # Both members of each pair run in the SAME invocation and window: comparing a
    # small row from one run against a large row from another would reintroduce the
    # cross-window error these pairs exist to remove.
    #
    # MEASURED OUTCOMES, recorded here because this is where the next person looks and
    # because a stale label in exactly this position (`"= production"` on the wrong LZF
    # arm) cost me a wrong claim earlier today. --rounds 31, LANG=en_US.UTF-8, live
    # incumbent in the same invocation, normalised against get_control only when the
    # control was itself admissible:
    #
    #   xrange_2      NEVER CERTIFIED in 2 attempts: STRADDLES-1 (CI [0.9665, 1.0075]),
    #                 then NULL-FAILED (null_redis 1.0204). Too short to measure.
    #   xrange_64     1.1206 / 1.1229 raw (0.2% apart), ADMISSIBLE both; 1.0124 normalised
    #   geosearch_2   1.0202 raw ADMISSIBLE, 0.9162 normalised -- fr BEHIND the control
    #   geosearch_64  1.1240 raw ADMISSIBLE, 1.0094 normalised -- fr AHEAD. An inversion.
    #   hgetall_3     1.1722 raw ADMISSIBLE, 1.0641 normalised
    #   hgetall_64    1.5466 raw ADMISSIBLE, 1.4040 normalised (raw replicated 3x within 1.3%)
    #
    # THE PATTERN THAT MATTERS: all four large-N siblings certified. Only two of the
    # four small-N originals did -- sort_ro_alpha (elsewhere) and xrange_2 each failed
    # to certify, for DIFFERENT reasons. So the board's small-n rows are not merely
    # misleading about the command; half of the ones tested cannot be certified at all,
    # and they have been quoted regardless.
    "sizepairs": [
        ("xrange_2", ["XADD xst 1-1 f v", "XADD xst 1-2 f v"],
         ["XRANGE", "xst", "-", "+"]),
        ("xrange_64",
         ["XADD xst64 %d-1 f v" % (i + 1) for i in range(64)],
         ["XRANGE", "xst64", "-", "+"]),
        ("geosearch_2",
         ["GEOADD g 13.361389 38.115556 P1", "GEOADD g 15.087269 37.502669 P2"],
         ["GEOSEARCH", "g", "FROMLONLAT", "15", "37", "BYRADIUS", "200", "km", "ASC"]),
        ("geosearch_64",
         ["GEOADD g64 " + " ".join(
             f"{13.0 + (i % 8) * 0.25} {37.0 + (i // 8) * 0.25} M{i:02d}"
             for i in range(64))],
         ["GEOSEARCH", "g64", "FROMLONLAT", "15", "37", "BYRADIUS", "500", "km", "ASC"]),
        ("hgetall_3", ["HSET h f1 v1 f2 v2 f3 v3"], ["HGETALL", "h"]),
        ("hgetall_64",
         ["HSET h64 " + " ".join(f"f{i:02d} v{i:02d}" for i in range(64))],
         ["HGETALL", "h64"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "sortsize": [
        ("sort_ro_alpha", ["RPUSH sl c a b"], ["SORT_RO", "sl", "ALPHA"]),
        ("sort_ro_alpha_64",
         ["RPUSH sl64 " + " ".join(f"w{i:02d}{'Ab'[i % 2]}" for i in range(64))],
         ["SORT_RO", "sl64", "ALPHA"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "unswept4": [
        ("xlen", ["XADD xst 1-1 f v", "XADD xst 1-2 f v"], ["XLEN", "xst"]),
        ("xrange_2", ["XADD xst 1-1 f v", "XADD xst 1-2 f v"], ["XRANGE", "xst", "-", "+"]),
        ("geopos", ["GEOADD g 13.361389 38.115556 P1"], ["GEOPOS", "g", "P1"]),
        ("geosearch", ["GEOADD g 13.361389 38.115556 P1", "GEOADD g 15.087269 37.502669 P2"],
         ["GEOSEARCH", "g", "FROMLONLAT", "15", "37", "BYRADIUS", "200", "km", "ASC"]),
        ("geoadd_same", ["GEOADD g 13.361389 38.115556 P1"],
         ["GEOADD", "g", "13.361389", "38.115556", "P1"]),
        ("pfadd_same", ["PFADD hll a b c"], ["PFADD", "hll", "a"]),
        ("pfcount", ["PFADD hll a b c d e"], ["PFCOUNT", "hll"]),
        ("sort_ro_alpha", ["RPUSH sl c a b"], ["SORT_RO", "sl", "ALPHA"]),
        # (frankenredis-r9mqp) The n=3 sibling above is an INTERCEPT measurement
        # wearing the command's name, and this harness had only that one. A SORT of
        # three elements does ~3 comparisons, so per-element cost and per-comparison
        # cost are the same number and the fixed per-command cost dominates the whole
        # row. Measured on the instruction sibling: fr's per-element collation is 34%
        # CHEAPER than redis's and fr is AHEAD from n=7 up (0.6972x at n=64), while at
        # n=3 it reads 1.49x BEHIND. A board carrying only the n=3 point therefore
        # reports SORT as fr's worst route when fr wins it at every realistic length.
        # This 64-element variant gives the board a second point so the crossover is
        # visible instead of inferred. Mixed case so COLLATION decides, not byte order.
        ("sort_ro_alpha_64",
         ["RPUSH sl64 " + " ".join(f"w{i:02d}{'Ab'[i % 2]}" for i in range(64))],
         ["SORT_RO", "sl64", "ALPHA"]),
        ("sintercard2", ["SADD s1 m1 m2 m3", "SADD s2 m2 m3 m4"], ["SINTERCARD", "2", "s1", "s2"]),
        ("smismember2", ["SADD st m1 m2 m3"], ["SMISMEMBER", "st", "m1", "nope"]),
        ("zrangebylex", ["ZADD z 0 a 0 b 0 c"], ["ZRANGEBYLEX", "z", "-", "+"]),
        ("zcount", ["ZADD z 1 a 2 b 3 c"], ["ZCOUNT", "z", "1", "3"]),
        ("hrandfield_c", ["HSET h f1 v1"], ["HRANDFIELD", "h", "1"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # (frankenredis-32f3p) The routes ozrro's gap metric left alone that turned out
    # to carry large parse counts. A big dispatch prize is NOT the same as being
    # below parity end to end, so they get a wall-clock row before anyone acts.
    # All cleared shape_admission_probe on both engines: 6 admitted, 0 rejected.
    # (frankenredis-2e4tq) The arity mis-claim family: a floor class keyed on
    # ARITY ALONE whose arm runs a KEYWORD-discriminating parser, so the sibling
    # option at that arity is claimed, declines, and falls through to the generic
    # dispatcher. Attributed on instructions (ZRANGE REV 3270.5 dispatch against
    # WITHSCORES 671.0; LPOS COUNT 2920.5 against base 466.9) but never clocked --
    # frankenredis-ailri is the standing reason those are different claims. Each
    # mis-claimed form is paired with the sibling that IS accepted, so the row is
    # read against its own base rather than a global mean.
    # MEASURED 2026-08-16 on ELF a146507d78bdb55610c63397 -- the whole family has
    # crossed, and none of these rows is sub-parity any more:
    #
    #   lpos_count_opt  1.1821 [1.1610, 1.2571]  nulls 1.0066/1.0286
    #                   1.2483 [1.2230, 1.2743]  nulls 1.0099/1.0244
    #     Both runs' nulls are off in the SAME direction by similar amounts and the
    #     intervals overlap, which is the full excusability condition. Worst bound
    #     1.1610. Was 0.9100 / 0.9361 / 0.9537 before 280383780 served LPOS COUNT
    #     in the arm -- a peer's fix on a route this file attributed.
    #
    #   zrange_rev      1.1483 [1.1249, 1.1825] ADMISSIBLE  nulls 1.0123/0.9996
    #                   1.1769 [1.1369, 1.2221]             nulls opposite
    #     THIRD ELF for the jnf09 crossing (0.8658 -> 1.1570 on ELF1), now with an
    #     admissible row on a binary built for an unrelated lever. Worst bound
    #     1.1249.
    #
    #   zrange_ws 1.1800 ADMISSIBLE, lpos_base 1.1290 -- the accepted siblings are
    #   ahead too, so no reading in this set is behind the incumbent.
    #
    # REPLICATED on a SECOND ELF ab969ddb4dd88322db2c7809:
    #   zrange_rev      1.1315 [1.0871, 1.1710] ADMISSIBLE  nulls 1.0033/1.0024
    #   lpos_count_opt  1.1991 [1.1591, 1.2418]  nulls 0.9943/1.0280 (opposite)
    #   zrange_ws       1.1857 [1.1717, 1.2169] ADMISSIBLE
    #   lpos_base       1.1075 [1.0878, 1.1358] ADMISSIBLE
    #
    # STANDING, stated per route rather than for the family as a whole:
    #   zrange_rev      REPLICATED STANDING -- ADMISSIBLE on BOTH ELFs (1.1483,
    #                   1.1315). WORST BOUND 1.0871.
    #   zrange_ws       ADMISSIBLE on both ELFs. Worst bound 1.1543.
    #   lpos_base       ADMISSIBLE on ELF2. Worst bound 1.0878.
    #   lpos_count_opt  three agreeing readings across two ELFs (1.1821, 1.2483,
    #                   1.1991) but NO admissible row -- two excusable on ELF1, and
    #                   ELF2's nulls are opposite. Worst bound 1.1591. Ahead beyond
    #                   doubt; not admissible-certified.
    # (frankenredis-u5cmn) Does the per-command active-expire cycle explain
    # expire_nx? frankenredis-bk7pi early-exits run_active_expire_cycle when
    # count_expiring_keys() == 0. expire_nx SEEDS a TTL, so the guard never fires
    # and every command runs a full cycle -- while upstream runs one per event
    # loop (whjrj: 150 fr call sites against 2 in server.c).
    #
    # ORDER IS THE CONTROL. seed() does not FLUSHALL, so keyspace state
    # accumulates across shapes. The no-TTL probe MUST run first, before any shape
    # seeds an expiry; once expire_nx_ttl has seeded, count_expiring_keys() stays
    # non-zero for the rest of the sweep.
    #
    # Both probes issue the SAME command shape against the SAME dispatch route and
    # executor. The only difference is whether a TTL exists anywhere, which is
    # exactly the condition the guard tests. If the gap tracks TTL presence, the
    # cycle is the cost; if both read alike, it is not and the lead dies.
    # (frankenredis-kvuyy follow-up) expire_nx is sub-parity at ~0.95 with dispatch
    # ruled out (classified, served, nothing allocating on the fast path) and the
    # active-expire cycle ruled out by ttlprobe. So is the cost the CONDITIONAL, or
    # EXPIRE generally?
    #
    # Same command, same key, same TTL state; the only difference is whether a
    # condition token is present, which changes arity 3 -> 4 and class Expire ->
    # ExpireCond. Both are classified and both are served, so this isolates the
    # conditional work rather than the routing.
    #   plain at parity + NX behind  -> the cost is the conditional path
    #   both behind                  -> the cost is EXPIRE itself, and expire_nx
    #                                   was never a conditional story
    #
    # ANSWERED, and it is the first branch. ELF a146507d78bdb55610c63397:
    #   run 1, HOST LOAD 12.11/64 (19%)
    #     expire_plain    1.0789 [1.0289, 1.1103]  nulls 1.0276/0.9988
    #     expire_nx_cond  0.9315 [0.8993, 0.9637]  nulls 0.9937/1.0038  ADMISSIBLE
    #   run 2, HOST LOAD 10.26/64 (16%)  -- 3 of 3 admissible, 0 null-failed
    #     expire_plain    1.1273 [1.0860, 1.1595]  nulls 1.0066/0.9994  ADMISSIBLE
    #     expire_nx_cond  0.9623 [0.9124, 0.9968]  nulls 1.0015/1.0092  ADMISSIBLE
    #
    # Plain EXPIRE is AHEAD (1.08-1.13, worst bound 1.0289); the conditional form
    # is BEHIND (0.93-0.96, worst bound 0.8993). Intervals disjoint in BOTH
    # pairings, two admissible rows per arm. The conditional path costs ~16%
    # relative to plain, on the same command, key and TTL state.
    #
    # THE CAUSAL READING WAS WRONG (frankenredis-kdehn). The measurement stands;
    # what it means does not. execute_plain_expire_borrowed is 15 lines and
    # DELEGATES to execute_plain_expire_kind_borrowed, so plain and conditional
    # share ONE executor and both call pttl_no_stats. Worse, the benchmarked NX
    # shape breaks at the `nx && remaining.is_some()` guard BEFORE
    # expire_at_milliseconds -- so it does strictly LESS store work than plain,
    # which performs the pttl read AND the set. A path doing less work cannot be
    # slower because of that work.
    #
    # The two shapes also differ in OUTCOME: plain returns 1 having set a TTL, the
    # NX form returns 0 having set nothing. So this pair does not isolate "the
    # conditional" at all -- it compares a successful write against a rejected one.
    #
    # A fair control is an NX that SUCCEEDS (key with no existing TTL), against
    # plain on the same key. Until that is run, the 1.08-vs-0.93 gap has no
    # attributed cause: dispatch is ruled out, the expire cycle is ruled out
    # (kvuyy), the shared executor is ruled out here, and the parsers differ only
    # by the one bulk that arity 4 inherently carries.
    # (frankenredis-kdehn) THE FAIR CONTROL. My earlier expirecond pair compared a
    # successful write (plain, returns 1, does pttl AND expire_at_milliseconds)
    # against a REJECTED one (NX on a key that already has a TTL, returns 0,
    # breaks before the set). That is unequal work, so it could not isolate the
    # condition.
    #
    # XX on a key that HAS a TTL succeeds on EVERY iteration -- the condition is
    # satisfied, the TTL is rewritten, and a TTL still exists for the next call.
    # So both shapes here return 1 and both perform the pttl read and the set.
    # The ONLY difference is the presence of a condition token and its check.
    #   both alike        -> the condition is free; the gap was the rejected write
    #   XX behind plain   -> the condition check itself is the cost
    #
    # ANSWERED: the condition costs. ELF a146507d78bdb55610c63397.
    #   run 1, HOST LOAD 23.30/64 (36%)  -- 0 of 3 admissible, UNTRUSTED
    #     expire_plain_ok 1.0921   expire_xx_ok 0.9610
    #   run 2, HOST LOAD 54.05/64 (84%)
    #     expire_plain_ok 1.0782 [1.0552, 1.1374] nulls 0.9873/1.0107 ADMISSIBLE
    #     expire_xx_ok    0.9693 [0.9229, 1.0410] nulls 0.9847/1.0058 STRADDLES-1
    #
    # Intervals DISJOINT (XX upper 1.0410 < plain lower 1.0552) and both runs agree
    # in direction and magnitude. On EQUAL work -- both return 1, both do the pttl
    # read and the set -- the conditional form is ~11% behind the plain one.
    #
    # PRECISE CLAIM: XX is below PLAIN. It is NOT demonstrably below parity: its CI
    # includes 1.0, so the honest statement is that the condition costs relative to
    # no condition, not that the route loses to the incumbent.
    #
    # Note run 2 certified at 84% load while run 1 failed at 36%. Admissibility,
    # not loadavg, decides which row to believe.
    # (frankenredis-bk8ag) The arity-6 ZADD two-flag form, which the ZaddTwoPair arm
    # claimed and dropped on generic until 7e5657839 chained zadd_flag2. Measured
    # ACROSS BUILDS rather than across bypass modes: fr-zadd predates the fix and
    # fr-zadd6 contains it, so the two binaries differ by the actual commit.
    # zadd_twopair is the reading the arm ALWAYS served -- it must not move.
    "zadd6": [
        ("zadd_twoflag", ["ZADD z6 1 m1"], ["ZADD", "z6", "XX", "CH", "5", "m1"]),
        ("zadd_twopair", ["ZADD z6 1 m1"], ["ZADD", "z6", "7", "m1", "8", "m2"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "expirefair": [
        ("expire_plain_ok", ["SET e v"], ["EXPIRE", "e", "500"]),
        ("expire_xx_ok", ["SET s v", "EXPIRE s 10000"], ["EXPIRE", "s", "500", "XX"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "expirecond": [
        ("expire_plain", ["SET e v"], ["EXPIRE", "e", "500"]),
        ("expire_nx_cond", ["SET s v", "EXPIRE s 10000"], ["EXPIRE", "s", "500", "NX"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "ttlprobe": [
        ("expire_nx_nottl", ["SET t v"], ["EXPIRE", "nosuch", "500", "NX"]),
        ("expire_nx_ttl", ["SET s v", "EXPIRE s 10000"], ["EXPIRE", "s", "500", "NX"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "misclaim": [
        ("zrange_ws", ["ZADD z 1 a 2 b 3 c"], ["ZRANGE", "z", "0", "-1", "WITHSCORES"]),
        ("zrange_rev", ["ZADD z 1 a 2 b 3 c"], ["ZRANGE", "z", "0", "-1", "REV"]),
        ("lpos_base", ["RPUSH l a b c d e"], ["LPOS", "l", "c"]),
        ("lpos_count_opt", ["RPUSH l a b c d e"], ["LPOS", "l", "c", "COUNT", "1"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "gaprejects": [
        ("hincrbyfloat", ["HSET h f 1"], ["HINCRBYFLOAT", "h", "f", "0"]),
        ("hsetnx_existing", ["HSET h f1 v1"], ["HSETNX", "h", "f1", "other"]),
        ("pfadd_existing", ["PFADD hll a b c"], ["PFADD", "hll", "a"]),
        ("sinter_2", ["SADD s1 m1 m2 m3", "SADD s2 m2 m3 m4"], ["SINTER", "s1", "s2"]),
        ("mget_3", ["MSET a 1 b 2 c 3"], ["MGET", "a", "b", "c"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # (frankenredis-ee41v) Fifth batch: single-command shapes no sweep had
    # touched. All 13 cleared shape_admission_probe on both engines. SCAN and KEYS
    # were probed and REJECTED: both engines return the same key SET in a
    # different ORDER, which is unspecified upstream and not a parity bug, but it
    # disqualifies them from a byte-exact throughput shape.
    # (frankenredis-50ntn) MEASURED 2026-08-16 on ELF 9a4ed1114443026df7a71030,
    # built locally with RCH_CARGO_WRAPPER_BYPASS=1 and --features
    # perf-ab-cascade-bypass (strings check = 1), so both arms are the SAME binary
    # and the comparison is the ROUTE, not the build:
    #
    #   zrangebyscore_l   generic path  0.9806 [0.9463, 0.9992]
    #                     fast route    1.2555 [1.2294, 1.2758]  and 1.2673 replicate
    #                     -> 1.280x, intervals DISJOINT, fast-arm runs 0.9% apart
    #
    #   sintercard_lim    generic       1.0434 [1.0126, 1.0742]  ADMISSIBLE
    #                     fast          1.0605 [1.0396, 1.0779]  ADMISSIBLE
    #
    # REPLICATED on a SECOND ELF ab969ddb4dd88322db2c7809 -- same source, different
    # compilation (codegen-units=1), so a one-off compilation artifact is ruled out:
    #
    #   zrangebyscore_l   generic 0.9876 [0.9605, 1.0073]  nulls 1.0072/0.9909
    #                     fast    1.2634 [1.2381, 1.2977]  ADMISSIBLE
    #                     -> 1.279x, against 1.280x on ELF1. Two ELFs, 0.1% apart.
    #   sintercard_lim    fast    1.0777 [1.0192, 1.1095]  ADMISSIBLE
    #
    # CERTIFIED CROSSING (frankenredis-50ntn). Four fast-route runs across two
    # independently compiled ELFs, TWO of them admissible:
    #     ELF1  1.2555 [1.2294, 1.2758]   1.2673 [1.2351, 1.2879]
    #     ELF2  1.2634 [1.2381, 1.2977] ADM   1.2834 [1.2685, 1.3039] ADM
    #   WORST BOUND across all four: 1.2294. Generic arm on both ELFs: 0.9806 and
    #   0.9876, intervals disjoint from every fast row.
    #
    # NEXT SUB-PARITY ROUTE, now with REPLICATED STANDING -- two ELFs, two
    # admissible rows, 0.06% apart, which is the tightest replication in this file:
    #     ELF2 ab969ddb  zadd_xx 0.8671 [0.8395, 0.8795] ADM  nulls 0.9913/0.9866
    #     ELF1 9a4ed111  zadd_xx 0.8676 [0.8416, 0.8873] ADM  nulls 1.0071/0.9912
    #   WORST BOUND 0.8395. Control-normalised ~0.77-0.79 (get_control 1.1217/1.0947).
    #
    # AFTER the arity-5 class landed (c2973aa12), on ELF a146507d78bdb55610c63397,
    # both arms the same binary. NO admissible row either side -- the host went
    # noisy, with nulls reaching 1.1100 -- so this is banked as SEPARATION, not as
    # a certified crossing:
    #
    #   generic/unclassified  0.8671  0.8676  0.8833  0.8696   spread 1.9%
    #     (first two are the pre-fix ELFs, both ADMISSIBLE; last two same-ELF)
    #   fast/classified       0.9813  1.0862  1.1380            spread 16%
    #
    #   min(fast) 0.9813 > max(generic) 0.8833. The populations do not overlap
    #   across 3 vs 4 runs. But the gap is 11% and the worst observed null
    #   deviation is also 11% (fr null 1.1100), so the separation sits AT the
    #   noise floor rather than above it. Re-measure when nulls pass.
    #
    #   Note which arm is noisy: the UNCLASSIFIED path is stable to 1.9% across
    #   three different ELFs, while the newly classified path varies 16% on one.
    #   That is worth explaining before claiming the crossing.
    #
    # EXPLAINED, and it was my method rather than the route. The fast-vs-generic
    # comparison was never ABBA: each invocation pits fr against redis, and I was
    # comparing two SEPARATE invocations, so host drift between them entered as
    # effect. Run back-to-back instead, fast then generic with nothing in between:
    #
    #   fast     1.1377 [1.0880, 1.2058]  nulls 0.9790/1.0299
    #   generic  0.8596 [0.8351, 0.9043]  nulls 0.9921/1.0070  ADMISSIBLE
    #   -> 1.324x, intervals DISJOINT by a wide margin.
    #
    # The fast arm's nulls tightened from 1.1100 to within 3%, and its two most
    # recent readings agree to four digits (1.1380, 1.1377). The 0.9813 outlier
    # came from the noisiest window. So the 16% spread was cross-invocation drift,
    # not an unstable route -- and the interleaved pairing is the one to trust.
    #
    # SECOND back-to-back pairing, independent of the first:
    #   fast     1.1414 [1.1176, 1.1570]  nulls 0.9860/0.9791 (same direction)
    #   generic  0.8623 [0.8391, 0.8903]  nulls 0.9805/0.9939  ADMISSIBLE
    #   -> 1.324x, identical to pairing 1's 1.324x, intervals DISJOINT.
    # Fast runs 1.1377 and 1.1414 overlap and sit 0.3% apart; generic 0.8596 and
    # 0.8623 are both admissible. Two pairings, same answer to four digits.
    #
    # zadd_xx FAST ROW IS NOW ADMISSIBLE: 1.1009 [1.0552, 1.1374] nulls
    # 1.0199/1.0044, on ELF a146507d78bdb55610c63397. With the generic arm
    # reproducing 0.8596-0.8833 every time it is measured, the crossing is
    # certified on both sides. Later same-session reading 1.1269 null-failed.
    #
    # NEW WORST ROUTE: expire_nx. Four readings on the same ELF --
    #   0.8817 [0.8217, 0.9280]  NULL-FAILED (nulls 0.9721/1.0495, opposite)
    #   0.9068                   NULL-FAILED
    #   0.9111                   NULL-FAILED
    #   0.9600 [0.9335, 0.9918]  ADMISSIBLE  (nulls 1.0141/1.0045)
    # Spread 9%, and the ONLY admissible row was the HIGHEST of the four, so
    # quoting it alone would have meant picking the most flattering reading.
    #
    # RESOLVED by a fifth run: 0.9291 [0.9132, 0.9561], nulls 1.0244/1.0289 --
    # same direction, similar amounts, which is the stated excusability condition,
    # and its interval OVERLAPS the admissible 0.9600. So there are now two
    # REPLICATED ON A SECOND ELF ab969ddb4dd88322db2c7809:
    #   expire_nx  0.9379 [0.9013, 0.9530] ADMISSIBLE  nulls 0.9924/1.0164
    # So expire_nx has ADMISSIBLE rows on TWO ELFs (0.9600 and 0.9379) with
    # overlapping intervals -- REPLICATED STANDING as a sub-parity route.
    # WORST BOUND 0.9013 across admissible rows.
    #
    # And an unplanned control for the ZADD lever: ELF2 predates c2973aa12, so it
    # has NO arity-5 class, and it reads zadd_xx 0.8455 [0.8304, 0.8777] (nulls
    # 0.9660/0.9816, same direction) -- agreeing with the pre-fix admissible pair
    # 0.8671/0.8676. The post-lever ELF reads 1.1009 ADMISSIBLE. That is the
    # crossing confirmed across two BUILDS differing by the actual commit, which is
    # stronger than the same-ELF bypass comparison it was first shown with.
    #
    # trustworthy rows and they agree:
    #
    #   trustworthy   0.9600 (ADM)   0.9291 (excusable)   -> 0.93-0.96
    #   null-failed   0.8817 (nulls OPPOSITE)  0.9068  0.9111
    #
    # The low readings are exactly the untrustworthy ones, and the second
    # trustworthy row landed at 0.93 rather than near 0.88 -- so the gate was
    # discriminating correctly, not flattering. Route is 0.93-0.96, worst bound
    # 0.9132 across trustworthy rows (0.8217 if the contaminated runs are counted,
    # which they should not be).
    #
    # expire_nx earlier history: 0.9068 and 0.9111 agree
    # in direction across the same two ELFs, but BOTH runs null-failed.
    #
    # Already attributed: ZADD length 5
    # is unclassified -- the class is `arity >= 8 && even` -- while
    # parse_borrowed_plain_zadd_flag_packet and execute_plain_zadd_flag_borrowed
    # both already exist, with zadd_incr covering INCR separately because its reply
    # is a bulk score rather than a count. The iqicb shape, not a mis-claim.
    #
    # zrangebyscore_l was 0.7932 [0.7713, 0.7980] before the floor class landed
    # (31b22f983) -- the deepest below-parity row I held, itself replicated across
    # two ELFs. It is now 26% ahead with an ADMISSIBLE row on ELF2 and disjoint
    # fast-vs-generic intervals on BOTH ELFs. ELF1's three runs all null-failed;
    # ELF2 supplies the admissible row, so the pair meets replicated standing.
    "unswept5": [
        ("getrange_full", ["SET s abcdefghijklmnop"], ["GETRANGE", "s", "0", "-1"]),
        ("lrange_neg", ["RPUSH l a b c d e"], ["LRANGE", "l", "-3", "-1"]),
        ("hmget_2", ["HSET h f1 v1 f2 v2"], ["HMGET", "h", "f1", "f2"]),
        ("mset_2", [], ["MSET", "ma", "1", "mb", "2"]),
        ("zadd_xx", ["ZADD z 1 a"], ["ZADD", "z", "XX", "1", "a"]),
        ("expire_nx", ["SET s v", "EXPIRE s 10000"], ["EXPIRE", "s", "500", "NX"]),
        ("lpos_rank", ["RPUSH l a b c d e"], ["LPOS", "l", "c", "RANK", "1"]),
        ("sintercard_lim", ["SADD s1 m1 m2 m3", "SADD s2 m2 m3 m4"],
         ["SINTERCARD", "2", "s1", "s2", "LIMIT", "1"]),
        ("sadd_existing2", ["SADD st m1 m2"], ["SADD", "st", "m1", "m2"]),
        ("zrangebyscore_l", ["ZADD z 1 a 2 b 3 c"],
         ["ZRANGEBYSCORE", "z", "1", "3", "LIMIT", "0", "2"]),
        ("hdel_existing", ["HSET h f1 v1"], ["HDEL", "h", "nofield2"]),
        ("append_empty", ["SET s abcdefghijklmnop"], ["APPEND", "s", ""]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "storeops": [
        ("exists_8key", ["MSET e1 1 e2 1 e3 1 e4 1 e5 1 e6 1 e7 1 e8 1"],
         ["EXISTS", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8"]),
        ("hmget_9field",
         ["HSET hm f1 v1 f2 v2 f3 v3 f4 v4 f5 v5 f6 v6 f7 v7 f8 v8 f9 v9"],
         ["HMGET", "hm", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9"]),
        ("zmscore_9member",
         ["ZADD zm 1 m1 2 m2 3 m3 4 m4 5 m5 6 m6 7 m7 8 m8 9 m9"],
         ["ZMSCORE", "zm", "m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m9"]),
        # (frankenredis-hwcm1) KEYSPACE-SENSITIVE — do NOT compare this row across
        # runs whose servers started with different ambient state. Measured directly:
        # fr/redis is 0.847 with 3 keys in the db and 0.800 with 28, on one pair of
        # live servers minutes apart, because fr's scan_in_db scales worse than
        # redis's. Every other shape in this set is bounded by its own key.
        #
        # That matters here because these servers start in the REPO ROOT and load
        # whatever `dump.rdb` is sitting there — proven the hard way when a stray
        # RDB made the redis arm refuse to start outright. So the db this row scans
        # is ambient, not fixture-controlled, and a moved/added/removed dump.rdb
        # shifts it. If you need a comparable scan row, control the starting db.
        ("scan_prefix", ["MSET tenant:needle:1 1 tenant:decoy:1 1 tenant:decoy:2 1"],
         ["SCAN", "0", "MATCH", "tenant:needle:*", "COUNT", "100"]),
        ("zunionstore_2key", ["ZADD za 1 a 2 b 3 c 4 d", "ZADD zb 1 b 2 c 3 d 4 e"],
         ["ZUNIONSTORE", "zdst", "2", "za", "zb"]),
        ("zinterstore_2key", ["ZADD za 1 a 2 b 3 c 4 d", "ZADD zb 1 b 2 c 3 d 4 e"],
         ["ZINTERSTORE", "zidst", "2", "za", "zb"]),
        ("bitop_and", ["SET ba abcdefghijklmnop", "SET bb ponmlkjihgfedcba"],
         ["BITOP", "AND", "bdst", "ba", "bb"]),
        ("bitop_not", ["SET ba abcdefghijklmnop"], ["BITOP", "NOT", "bndst", "ba"]),
        ("sunionstore_3src",
         ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
         ["SUNIONSTORE", "sudst", "sa", "sb", "sc"]),
        ("sinterstore_3src",
         ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
         ["SINTERSTORE", "sidst", "sa", "sb", "sc"]),
        ("sdiffstore_3src",
         ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
         ["SDIFFSTORE", "sddst", "sa", "sb", "sc"]),
        # ZMPOP pops, so it would drain any key it could reach; the missing-key form
        # returns a null array and creates nothing.
        ("zmpop_missing", [], ["ZMPOP", "1", "nosuchzset", "MIN"]),
        # Control: GET is untouched by any of the dispatch work these rows measure.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
}


def sha256_of(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def running_image_sha(pid: int) -> str:
    """SHA-256 of the image the process is ACTUALLY executing.

    Hashing the path we intended to launch would not catch a stale copy, a
    symlink, or a harness that launched the same binary twice and called one of
    them the candidate. Reading `/proc/<pid>/exe` reports what is running.
    """
    return sha256_of(f"/proc/{pid}/exe")


def observed_threads(pid: int) -> int:
    """Threads the server ACTUALLY has, not the number any flag requested."""
    return len(os.listdir(f"/proc/{pid}/task"))


def provenance(fr_pid: int, redis_pid: int) -> dict:
    governor = "unknown"
    gov_path = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"
    if os.path.exists(gov_path):
        with open(gov_path) as handle:
            governor = handle.read().strip()
    isa = []
    with open("/proc/cpuinfo") as handle:
        flags = handle.read()
    for feature in ("avx512f", "avx2", "avx", "sse4_2"):
        if re.search(rf"\b{feature}\b", flags):
            isa.append(feature)
    with open("/proc/loadavg") as handle:
        loadavg = " ".join(handle.read().split()[:3])
    # (frankenredis-eh2ct) CPU MHz, and it is a RANGE on purpose. This block stamped
    # loadavg but not clock, so every row taken from this harness had to have its
    # frequency sampled by hand afterwards or go into the ledger without one — and a
    # ratio without a frequency is not comparable across windows on this host. A single
    # mean would be worse than nothing here: /proc/cpuinfo has shown 1429 MHz and 4214 MHz
    # on different cores AT THE SAME INSTANT (2.9x), so the spread is the honest figure and
    # the mean alone would imply a stability the host does not have. Read once here and
    # once after the square finishes, so a governor step across the run is visible too.
    mhz = cpu_mhz_summary()
    return {
        "host": socket.gethostname(),
        "kernel": platform.release(),
        "cores": os.cpu_count(),
        "governor": governor,
        "isa": isa[0] if isa else "unknown",
        "loadavg": loadavg,
        "cpu_mhz": mhz,
        "fr_elf_sha256": running_image_sha(fr_pid),
        "redis_elf_sha256": running_image_sha(redis_pid),
        "fr_threads_observed": observed_threads(fr_pid),
        "redis_threads_observed": observed_threads(redis_pid),
    }


def normalised_bounds(row_ratio, row_ci, control_ratio, control_ci):
    """Normalised point estimate plus the WORST and BEST bounds its intervals allow.

    (frankenredis-eh2ct) THE CONVENTION, MOVED OUT OF PEOPLE'S HEADS. A thin normalised
    margin is quoted per the replicated-standing convention as the most PESSIMISTIC pairing
    the two intervals allow — row CI-low over control CI-high — and a point estimate on its
    own has twice been read as a crossing when the bounds bracket 1.0. `geosearch_64` was
    banked at 1.0094 normalised and called an inversion; three replicates later put it at
    0.9806 / 0.9937 / 1.0044 with a worst bound of 0.9549, i.e. never a crossing at all.
    Computing this by hand is what allowed the point estimate to travel alone, so the
    harness computes it now.

    Returns (point, worst, best, verdict). STRADDLES-1 means the intervals admit both
    directions and NO crossing may be claimed in either.
    """
    point = row_ratio / control_ratio
    worst = row_ci[0] / control_ci[1]
    best = row_ci[1] / control_ci[0]
    if worst > 1.0:
        verdict = "AHEAD"
    elif best < 1.0:
        verdict = "BEHIND"
    else:
        verdict = "STRADDLES-1"
    return point, worst, best, verdict


def relative_ci_width_pct(ratio, ci) -> float:
    """Interval width as a percentage of the point estimate."""
    if not ratio:
        return float("nan")
    return (ci[1] - ci[0]) / ratio * 100.0


def normaliser_is_wider(row_ratio, row_ci, control_ratio, control_ci) -> bool:
    """Is the denominator noisier than the row it is meant to stabilise?

    (frankenredis-eh2ct) MEASURED, not assumed: over six runs `get_control`'s interval ran
    5.8 / 4.0 / 2.9 / 3.1 pct against `geosearch_64`'s 2.0 / 2.6 / 2.1 / 0.8, and doubling
    the rounds narrowed the SHAPE to 0.8 pct while leaving the control unchanged — sampling
    noise shrinks with n, drift does not. When this is true the normalised figure inherits
    more variance than it removes, and a sub-1 pct margin on it is not adjudicable at any
    round count. Worth saying out loud on the row rather than leaving the reader to compare
    two intervals by eye.
    """
    return (relative_ci_width_pct(control_ratio, control_ci)
            >= relative_ci_width_pct(row_ratio, row_ci))


def cpu_mhz_summary() -> str:
    """Per-core clock as `mean/min/max`, or `unknown` where the kernel does not report it.

    Returns a STRING rather than a number because the consumer is a provenance line and
    the three figures must travel together; a caller that wants one of them can split it.
    Never raises: a missing or malformed `cpu MHz` field degrades the provenance line, and
    degrading provenance must not abort a measurement that is otherwise sound.
    """
    try:
        with open("/proc/cpuinfo") as handle:
            values = [float(line.split(":", 1)[1])
                      for line in handle
                      if line.startswith("cpu MHz")]
    except (OSError, ValueError, IndexError):
        return "unknown"
    if not values:
        return "unknown"
    return "mean %.0f min %.0f max %.0f (spread %.2fx)" % (
        sum(values) / len(values), min(values), max(values),
        max(values) / min(values) if min(values) else float("nan"))


def wait_ready(port: int, timeout_s: float = 30.0,
               proc: subprocess.Popen | None = None) -> None:
    """(frankenredis-yaul4) Fail on a DEAD server immediately, and say so.

    Without the `proc` check a server that exits during startup is only noticed
    30s later, and then only as a timeout -- or worse, the run proceeds and
    provenance dies reading /proc/<pid>/exe of a corpse, which surfaces as a bare
    FileNotFoundError naming a pid and nothing else. That is exactly how the
    poisoned repo-root dump.rdb presented here.
    """
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if proc is not None and proc.poll() is not None:
            raise SystemExit(
                f"server on port {port} exited during startup with rc="
                f"{proc.returncode} before answering PING -- it never ran, so "
                f"there is no measurement here to interpret")
        probe = subprocess.run([CLI, "-p", str(port), "ping"],
                               capture_output=True, text=True)
        if probe.returncode == 0 and "PONG" in probe.stdout:
            return
        time.sleep(0.2)
    raise SystemExit(f"server on port {port} never became ready")


def free_port() -> int:
    """Bind port 0 and hand back what the kernel assigned."""
    probe = socket.socket()
    try:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]
    finally:
        probe.close()


def assert_ours(port: int, proc: subprocess.Popen, label: str) -> None:
    """(frankenredis-yaul4) The process answering on `port` must be the one we
    started. PING proves only that SOMETHING listens: with the old fixed ports a
    peer's server answered, our own engine exited unable to bind, and the run was
    one step away from reporting a ratio measured on somebody else's binary. The
    ELF sha in provenance() does not catch this -- it shas OUR pid, not the pid
    that actually served the traffic."""
    out = subprocess.run([CLI, "-p", str(port), "info", "server"],
                         capture_output=True, text=True)
    match = re.search(r"process_id:(\d+)", out.stdout)
    if not match:
        raise SystemExit(f"{label} on port {port}: INFO server carried no process_id")
    served_by = int(match.group(1))
    if served_by != proc.pid:
        raise SystemExit(
            f"{label} on port {port} is served by pid {served_by}, not the "
            f"process we launched (pid {proc.pid}) -- another agent holds that "
            f"port, so any ratio from this run would describe their binary")


def seed(port: int, commands: list[str]) -> None:
    for command in commands:
        subprocess.run([CLI, "-p", str(port)] + command.split(),
                       capture_output=True, text=True, check=False)


def error_probe(port: int, argv: list[str], engine: str, label: str) -> None:
    """A refused command reads as enormous throughput, so refuse to time it.

    `redis-benchmark` counts an error reply as a completed request. A shape that
    one engine rejects therefore produces a fast, confident, meaningless number.
    """
    out = subprocess.run([CLI, "-p", str(port)] + argv,
                         capture_output=True, text=True).stdout.strip()
    if out.startswith(("ERR", "WRONGTYPE", "NOPERM")) or "unknown command" in out:
        raise SystemExit(f"error probe failed: {engine} rejects `{label}`: {out}")


RPS = re.compile(r"([0-9]+\.[0-9]+) requests per second")


def time_slot(port: int, argv: list[str], ops: int, pipeline: int,
              client_core: str | None) -> float:
    """One timed slot: ops/s for a single redis-benchmark invocation."""
    cmd = [BENCH, "-p", str(port), "-n", str(ops), "-c", "1",
           "-P", str(pipeline), "-q"] + argv
    if client_core:
        cmd = ["taskset", "-c", client_core] + cmd
    out = subprocess.run(cmd, capture_output=True, text=True).stdout
    match = None
    for match in RPS.finditer(out):
        pass
    if match is None:
        raise SystemExit(f"redis-benchmark produced no rate for {argv}:\n{out}")
    return float(match.group(1))


def bootstrap_ci(values: list[float], iters: int = 2000,
                 seed_value: int = 20260814) -> tuple[float, float]:
    rng = random.Random(seed_value)
    n = len(values)
    medians = sorted(
        statistics.median(rng.choices(values, k=n)) for _ in range(iters)
    )
    return medians[int(0.025 * iters)], medians[int(0.975 * iters)]


def run_row(label: str, fr_port: int, redis_port: int, argv: list[str],
            rounds: int, ops: int, pipeline: int,
            client_core: str | None) -> dict:
    # Warm both arms once so neither pays first-touch inside a measured slot.
    time_slot(fr_port, argv, max(ops // 10, 1000), pipeline, client_core)
    time_slot(redis_port, argv, max(ops // 10, 1000), pipeline, client_core)

    ratios, null_redis, null_fr = [], [], []
    for _ in range(rounds):
        a_slots, b_slots = [], []
        for slot in SQUARE:
            if slot == "A":
                a_slots.append(time_slot(redis_port, argv, ops, pipeline, client_core))
            else:
                b_slots.append(time_slot(fr_port, argv, ops, pipeline, client_core))
        # ops/s, so fr/redis > 1 means fr is faster.
        ratios.append(statistics.median(b_slots) / statistics.median(a_slots))
        # Each arm's own first-half / second-half ratio. The square places the
        # halves symmetrically, so a departure from 1.0 is drift or contention,
        # not slot position.
        null_redis.append(statistics.median(a_slots[:2]) / statistics.median(a_slots[2:]))
        null_fr.append(statistics.median(b_slots[:2]) / statistics.median(b_slots[2:]))

    ratio = statistics.median(ratios)
    low, high = bootstrap_ci(ratios)
    n_redis, n_fr = statistics.median(null_redis), statistics.median(null_fr)
    verdict, binding = classify_row(ratio, low, high, n_redis, n_fr)
    return {
        "label": label,
        "ratio": ratio,
        "ci": (low, high),
        "null_redis": n_redis,
        "null_fr": n_fr,
        "verdict": verdict,
        "binding": binding,
    }


def main(argv_in: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fr-bin", required=False)
    parser.add_argument("--shapes", default="cascade")
    parser.add_argument("--rounds", type=int, default=9)
    parser.add_argument("--ops", type=int, default=50000)
    parser.add_argument("--pipeline", type=int, default=16)
    # (frankenredis-yaul4) Ephemeral by default. The fixed pair 27841/27842 was
    # found held by ANOTHER agent's run (an fr_post binary and its redis), so
    # our own fr could not bind and exited while the squatter answered PING.
    parser.add_argument("--fr-port", type=int, default=0)
    parser.add_argument("--redis-port", type=int, default=0)
    parser.add_argument("--client-core", default=None)
    # (frankenredis-xvq1a) Optional server pinning. The per-arm nulls are each
    # arm's own first half over its second half — WITHIN-process drift — while the
    # reported ratio is a CROSS-process comparison, so placement between the two
    # server processes is a term the nulls structurally cannot see. Whether that
    # matters is WORKLOAD-DEPENDENT and was measured both ways on this host:
    #   * RESTORE decode (long ~40ms DEBUG-driven bursts, one connection): a
    #     cross-process A/A between two identical fr servers scattered 0.918-1.058
    #     over six invocations; pinning to symmetric core sets collapsed it to
    #     1.0106 [1.0015, 1.0159] and 1.0081 [0.9847, 1.0260].
    #   * THIS harness's redis-benchmark workload (-c1 -P16, many short ops):
    #     unpinned cross-process A/A came out 0.9974 and 1.0106, and PINNING DID
    #     NOT IMPROVE IT (1.0151 and 0.9896, one row null-failed). So the term is
    #     ~1% here and these flags buy nothing for the registered shape sets.
    # They are kept because the RESTORE result shows the term is real for other
    # workloads, and because a future shape set may be burst-shaped. Off by
    # default: do not pin without first showing --cross-null needs it.
    parser.add_argument("--fr-core", default=None,
                        help="taskset core list for the fr server (e.g. 0-3); "
                             "measured to buy nothing on the current shape sets")
    parser.add_argument("--redis-core", default=None,
                        help="taskset core list for the redis server; use a set "
                             "symmetric with --fr-core (same CCD, same size)")
    # Lets the harness MEASURE its own cross-process null instead of assuming it:
    # run the second arm as another fr, so the reported ratio should be 1.0. This
    # is the flag that settled the question above, and it is the one worth using.
    parser.add_argument("--cross-null", action="store_true",
                        help="replace the redis arm with a SECOND fr server; the "
                             "reported ratio is then a cross-process A/A and must "
                             "come out 1.0. Not a competitive row.")
    parser.add_argument("--expect-elf", default=None)
    parser.add_argument("--list", action="store_true")
    # (frankenredis-enrhw) Decision-rule test. Pure computation: no server, no
    # timing, no build — so it runs under a build halt, which is exactly when a
    # change to a gate most needs checking.
    parser.add_argument("--selftest", action="store_true")
    # (frankenredis-eh2ct) Server-free audit: which rows measure an intercept.
    parser.add_argument("--audit-sizes", action="store_true")
    # (frankenredis-eh2ct) Certify a SUBSET of a set's shapes. A small/large pair plus
    # its control is the whole question for an intercept check, and a 7-shape set at
    # the round count admissibility actually needs (21-31) overruns the wall-clock
    # budget of a single run — which is how one attempt got killed with no output at
    # all. Filtering keeps both members of a pair in ONE invocation, which is the
    # property that matters; splitting them across runs would reintroduce the
    # cross-window error the pairs exist to remove.
    parser.add_argument("--only", default=None,
                        help="comma-separated shape labels to keep from --shapes")
    args = parser.parse_args(argv_in)

    if args.selftest:
        # Both pure-computation rules under one flag: the verdict classifier and the
        # size parser that decides which rows are intercept rows.
        rc = _selftest_classify()
        print()
        return rc | _selftest_sizes()
    if args.audit_sizes:
        return audit_shape_sizes()
    if args.list:
        for name, shapes in SHAPE_SETS.items():
            print(f"{name}: {', '.join(label for label, _, _ in shapes)}")
        return 0

    if not args.fr_bin:
        parser.error("--fr-bin is required")
    for path in (BENCH, REDIS, CLI, args.fr_bin):
        if not os.path.exists(path):
            raise SystemExit(f"missing binary: {path}")
    shapes = SHAPE_SETS.get(args.shapes)
    if shapes is not None and args.only:
        wanted = [w.strip() for w in args.only.split(",") if w.strip()]
        by_label = {label: (label, seeds, argv) for label, seeds, argv in shapes}
        missing = [w for w in wanted if w not in by_label]
        if missing:
            parser.error(
                "--only names shapes not in set %r: %s (available: %s)"
                % (args.shapes, ", ".join(missing),
                   ", ".join(label for label, _s, _a in shapes))
            )
        shapes = [by_label[w] for w in wanted]
    if shapes is None:
        raise SystemExit(f"unknown shape set {args.shapes}; try --list")

    # (frankenredis-yaul4) 0 means "pick a free one"; an explicit --fr-port /
    # --redis-port is still honoured, and identity-checked either way.
    if args.fr_port == 0:
        args.fr_port = free_port()
    if args.redis_port == 0:
        args.redis_port = free_port()

    def pinned(core: str | None, cmd: list[str]) -> list[str]:
        return (["taskset", "-c", core] + cmd) if core else cmd

    if (args.fr_core is None) != (args.redis_core is None):
        parser.error("--fr-core and --redis-core must be given together; pinning "
                     "one server and not the other is worse than pinning neither")

    # (frankenredis-yaul4) Both engines run in a private directory. Bare,
    # they inherit this process's cwd -- normally the repo root, shared by a dozen
    # agents -- and load whatever dump.rdb is sitting there. An fr-written
    # dump.rdb carrying a FUNCTION library redis 7.2.4 refuses makes redis abort
    # during startup, which reached this harness as a FileNotFoundError on
    # /proc/<pid>/exe inside provenance. A perf harness that cannot boot its
    # incumbent has no ratio to report.
    workdir = tempfile.mkdtemp(prefix="fr_balanced_square_")
    fr_bin = os.path.abspath(args.fr_bin)
    fr = subprocess.Popen(
        pinned(args.fr_core, [fr_bin, "--port", str(args.fr_port),
                              "--save", "", "--appendonly", "no"]),
        cwd=workdir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    second_arm = ([fr_bin, "--port", str(args.redis_port),
                   "--save", "", "--appendonly", "no"] if args.cross_null
                  else [os.path.abspath(REDIS), "--port", str(args.redis_port),
                        "--save", "", "--appendonly", "no"])
    redis = subprocess.Popen(pinned(args.redis_core, second_arm), cwd=workdir,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_ready(args.fr_port, proc=fr)
        wait_ready(args.redis_port, proc=redis)
        assert_ours(args.fr_port, fr, "fr")
        assert_ours(args.redis_port, redis, "second arm")
        prov = provenance(fr.pid, redis.pid)
        if args.expect_elf and not prov["fr_elf_sha256"].startswith(args.expect_elf):
            raise SystemExit(
                f"ELF mismatch: running image is {prov['fr_elf_sha256'][:16]}, "
                f"expected {args.expect_elf}")

        print("== provenance (self-reported from inside the running processes) ==")
        for key, value in prov.items():
            print(f"  {key:24} {value}")
        if args.cross_null:
            print("\n== CROSS-PROCESS A/A MODE: the second arm is another fr, so "
                  "every ratio below must be 1.0. NOT a competitive row. ==")
        if args.fr_core:
            print(f"  servers pinned: fr={args.fr_core} redis={args.redis_core}")
        else:
            print("  servers unpinned (the per-arm nulls are within-process drift "
                  "and do not bound cross-process placement; run --cross-null to "
                  "measure that term rather than assume it — on this workload it "
                  "measured ~1%)")
        print(f"\nsquare={SQUARE}  rounds={args.rounds}  ops/slot={args.ops}"
              f"  -P{args.pipeline}  null bound +/-{NULL_BOUND}")
        # (fleet finding, 2026-08-16) Host load was already CAPTURED into the
        # environment dict and never printed, so no banked row carried it. torch read
        # zero-certified across 21 lanes for four ticks on contention alone, and the
        # docstring above records this repo measuring A/A nulls of 0.85-1.07 at
        # loadavg 58 -- a same-binary comparison whose true value is exactly 1.0000.
        # A row that fails to certify under load is not a loss until it has been
        # re-run in a quiet window, and that judgement is impossible after the fact
        # unless the load is printed with the row.
        try:
            with open("/proc/loadavg") as _fh:
                _l = _fh.read().split()
            _n = os.cpu_count() or 1
            print(f"  HOST LOAD {_l[0]} {_l[1]} {_l[2]} on {_n} cpus"
                  f"  ({float(_l[0]) / _n * 100:.0f}% of 1-min capacity)"
                  f"  -- quote this with every row")
        except Exception:
            print("  HOST LOAD unavailable")
        # (frankenredis-eh2ct) Same argument as HOST LOAD, one variable along: this host
        # runs powersave and its cores sit at DIFFERENT frequencies simultaneously, so a
        # ratio without a clock is not comparable across windows. Printed here at the
        # start and again after the last row, because a governor step DURING the square is
        # exactly the drift the per-arm nulls are there to catch and the reader should be
        # able to see it in the same output.
        print(f"  CPU MHz (before) {prov['cpu_mhz']}")

        rows = []
        for label, seeds, bench_argv in shapes:
            seed(args.fr_port, seeds)
            seed(args.redis_port, seeds)
            error_probe(args.fr_port, bench_argv, "fr", label)
            error_probe(args.redis_port, bench_argv, "redis", label)
            rows.append(run_row(label, args.fr_port, args.redis_port, bench_argv,
                                args.rounds, args.ops, args.pipeline,
                                args.client_core))
            row = rows[-1]
            print(f"  {row['label']:<14} {row['ratio']:.4f}"
                  f"  [{row['ci'][0]:.4f}, {row['ci'][1]:.4f}]"
                  f"  nulls {row['null_redis']:.4f}/{row['null_fr']:.4f}"
                  f"  {row['verdict']} [{row['binding']}]")

        print(f"  CPU MHz (after)  {cpu_mhz_summary()}")
        print(f"\nRATIO = fr ops/s / redis ops/s   (>1 means FrankenRedis faster)")
        print(f"{'shape':<14}{'ratio':>9}{'95% CI':>22}"
              f"{'null redis':>12}{'null fr':>10}  verdict")
        for row in rows:
            ci_text = f"[{row['ci'][0]:.4f}, {row['ci'][1]:.4f}]"
            print(f"{row['label']:<14}{row['ratio']:>9.4f}{ci_text:>22}"
                  f"{row['null_redis']:>12.4f}{row['null_fr']:>10.4f}  "
                  f"{row['verdict']} [{row['binding']}]")
        admissible = [r for r in rows if r["verdict"] == "ADMISSIBLE"]
        print(f"\n{len(admissible)} of {len(rows)} rows admissible; "
              f"{sum(1 for r in rows if r['verdict'] == 'NULL-FAILED')} null-failed")
        # (frankenredis-eh2ct) Say whether a CONTROL-NORMALISED figure is even
        # available, because working that out by hand is how a bad one gets quoted.
        # Twice in one sitting a run came back with the row admissible and the control
        # refused, or the reverse: run 1 refused hgetall_64 and passed get_control,
        # run 2 passed hgetall_64 and refused get_control. Either way the normalised
        # value is UNAVAILABLE, and dividing an admissible row by an inadmissible
        # control — or pairing rows across two windows — is precisely the error the
        # same-invocation rule exists to prevent.
        control = next((r for r in rows if r["label"] == "get_control"), None)
        if control is None:
            print("normalised: n/a -- no get_control row in this selection")
        elif control["verdict"] != "ADMISSIBLE":
            print(f"normalised: n/a -- get_control is {control['verdict']}"
                  f" (ratio {control['ratio']:.4f}, nulls "
                  f"{control['null_redis']:.4f}/{control['null_fr']:.4f});"
                  " quote RAW ratios only from this run")
        else:
            print(f"normalised against get_control {control['ratio']:.4f}"
                  " (admissible), for rows that are themselves admissible:")
            print("  %-22s %8s %9s %9s  %s"
                  % ("shape", "point", "worst", "best", "verdict"))
            straddlers = []
            for row in admissible:
                if row["label"] == "get_control":
                    continue
                point, worst, best, verdict = normalised_bounds(
                    row["ratio"], row["ci"], control["ratio"], control["ci"])
                note = ""
                if normaliser_is_wider(row["ratio"], row["ci"],
                                       control["ratio"], control["ci"]):
                    note = ("  <- normaliser WIDER than the row (%.1f vs %.1f pct): it "
                            "injects more variance than it removes"
                            % (relative_ci_width_pct(control["ratio"], control["ci"]),
                               relative_ci_width_pct(row["ratio"], row["ci"])))
                if verdict == "STRADDLES-1":
                    straddlers.append(row["label"])
                print("  %-22s %8.4f %9.4f %9.4f  %s%s"
                      % (row["label"], point, worst, best, verdict, note))
            if straddlers:
                # The failure this prevents is specific and it has happened: a point
                # estimate of 1.0094 travelled as "an inversion, fr AHEAD" while its own
                # bounds ran 0.9549-1.0422.
                print("  NOTE: %s STRADDLE 1.0 -- their intervals admit both directions, so"
                      " NO crossing may be claimed either way. Quote the WORST bound."
                      % ", ".join(straddlers))
            refused = [r["label"] for r in rows
                       if r["verdict"] != "ADMISSIBLE" and r["label"] != "get_control"]
            if refused:
                print("  (no normalised figure for %s -- not admissible)"
                      % ", ".join(refused))
        return 0
    finally:
        for proc in (fr, redis):
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
