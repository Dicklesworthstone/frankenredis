#!/usr/bin/env python3
"""Rank class-[A] front-classification candidates: executor EXISTS, floor entry MISSING.

(frankenredis-ozrro) This exists because a name-based scan misled me three times, each in a
NEW way, and the third time cost a ~40-line executor that duplicated a working one:

  1. `corpus_coverage.py` matches `fn execute_plain_<cmd>_borrowed` per command and cannot see
     SHARED executors (`execute_plain_keymeta_borrowed(PlainKeyMetaCmd::Pexpiretime, ...)`).
     Acting on it, a duplicate PEXPIRETIME executor was written AND shipped (b2df577ba).
  2. `cascade_depth.py` ranks arms INSIDE the cascade and cannot see commands with no arm at
     all — which are exactly the ones on the GENERIC route. Reading its empty candidate list
     as "nothing left" produced a false "vein closed" verdict; six commands invisible to it
     were then measured and three were above parity.
  3. A per-command grep for `fn execute_plain_spop_borrowed` returned 0 for SPOP COUNT, whose
     executor is spelled `execute_plain_spop_count_borrowed` — same command, VARIANT spelling.
     Only an `assert "already applied"` in my own patch script stopped the duplicate landing.

The through-line: each scan answered a NARROWER question than the one asked of it, and each
time a zero was read as "absent" rather than "this scan cannot see it". Two rules follow, and
this tool applies both:

  * Match the command STEM, not a full name: `execute_plain_spop` finds base, count and any
    future variant. Exact-name matching is what missed #3.
  * A candidate is only worth work if it is ALSO deep in the cascade. An executor reached from
    arm 1 is already at the front, and a floor entry for it buys the depth saving of zero arms.
    Reporting "executor exists, floor entry missing" WITHOUT the depth is how a list of ten
    near-worthless candidates looks like a worklist.

WHAT IT FOUND when first written, AND WHY THAT VERDICT WAS WRONG. It filed hset (arm 16) and
mset (arm 19) as "marginal, below WORTH_IT" and declared class [A] exhausted. Both were then
MEASURED: 681.8 and 912.9 instr/op of dispatch, against the law's 461 and 596 — the law
under-predicts by ~50 pct at shallow arms, and WORTH_IT is DERIVED FROM that law, so the cutoff
was circular. Two of the hottest commands in any workload were buried by a threshold computed
from a biased fit. This tool now (a) prefers a MEASURED number over the fit wherever one
exists, (b) reports an unmeasured shallow arm as UNSIZED rather than as cheap, because a
prediction must never close a vein, and (c) flags HOT commands, since ranking by predicted size
alone is what surfaced PUBSUB (6,214 instr/op, issued by almost nobody) ahead of MSET and HSET.

Usage:
    floor_entry_candidates.py [--all]   # --all shows front-of-cascade candidates too
    floor_entry_candidates.py --self-test
"""

from __future__ import annotations

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FLOOR = os.path.join(ROOT, "crates", "fr-server", "src", "main.rs")
RUNTIME = os.path.join(ROOT, "crates", "fr-runtime", "src", "lib.rs")
CMD_TABLE = os.path.join(ROOT, "crates", "fr-command", "src", "lib.rs")

# Below this arm position the depth saving does not repay the review cost, and the
# cascade-depth law's intercept is not constrained there either. Same threshold
# cascade_depth.py uses, deliberately.
WORTH_IT = 30
SLOPE, INTERCEPT = 45.1, -261.0

# Refused by the borrowed admission guard or dispatched via subcommands, so a floor entry
# on the bare name serves nothing. Listed rather than silently dropped.
NOT_SERVABLE = {
    "xread", "xreadgroup", "xpending", "xinfo", "xgroup", "xautoclaim", "xclaim",
    "subscribe", "unsubscribe", "psubscribe", "punsubscribe", "ssubscribe", "sunsubscribe",
    "spublish", "pubsub", "multi", "exec", "discard", "watch", "unwatch", "reset",
}


def executor_stems(runtime_src: str) -> set[str]:
    """Every `execute_plain_<stem>_borrowed*` stem, so VARIANT spellings are visible."""
    return {
        m.group(1)
        for m in re.finditer(r"fn execute_plain_([a-z0-9_]+?)_borrowed", runtime_src)
    }


def has_executor(command: str, stems: set[str]) -> list[str]:
    """Executors serving `command`, matched on STEM so `spop` finds `spop_count`."""
    return sorted(s for s in stems if s == command or s.startswith(command + "_"))


def floor_classified(floor_src: str) -> set[str]:
    """Commands with a floor entry, keyed on the COMMAND enum (named after the command).

    (frankenredis-ozrro) The COMMAND enum is the reliable key — a CLASS is named for the SHAPE
    it serves (GeohashSingle, KeyedValuesWrite), not the command. But a command reached only
    through a PARAMETERISED class arm still appears here, because those arms match on
    `(array_len, BorrowedDispatchFloorCommand::Foo)` and so mention the command enum. That is
    why this works for EXISTS and the keyed-values six even though no literal `(N, Cmd)` tuple
    exists for them — a distinction that cost me a false "class [A] is exhausted" verdict when
    a DIFFERENT grep of mine looked for the literal tuples instead.
    """
    return {
        m.group(1).lower()
        for m in re.finditer(
            r"BorrowedDispatchFloorCommand::([A-Z][A-Za-z0-9]*)", floor_src
        )
    }


# (frankenredis-ozrro) MEASURED dispatch, instr/op, from banked ledger rows. The depth law
# UNDER-predicts — -48 pct at arm 16, -53 pct at arm 19, -14 pct at arm 127 — so where a real
# measurement exists it replaces the prediction outright. WORTH_IT is derived FROM that law,
# which made it circular: I used a law-derived cutoff to file MSET and HSET as "marginal" and
# closed the vein twice, burying a ~15 pct win on one of the hottest commands there is.
MEASURED_DISPATCH = {
    "mset": 912.9,
    "hset": 681.8,
    "pubsub": 6214.4,   # before its floor entry landed
}

# The CHEAPEST front-classified dispatch observed in this campaign (arity 2). Compared against
# deliberately, because it is a LOWER BOUND on what a floor entry costs: a candidate that beats
# even the cheapest possible destination by a clear margin is worth taking regardless of its
# arity, and one that does not needs its own arity band worked out before anyone bothers.
#
# An earlier draft of this file tried to pick a per-arity band here and keyed the lookup on the
# ARM POSITION instead of the arity, via an expression that silently always returned 306. Both
# candidates still cleared it, so the verdicts were right by luck — which is exactly how a
# broken comparison survives review. Using one documented bound is honest; faking per-arity
# precision from data this file does not have is not.
FRONT_CLASSIFIED_FLOOR_MIN = 306

# Commands hot enough that a small block still matters. Traffic is the axis this screen used to
# ignore entirely, which is how it surfaced PUBSUB (6,214 instr/op, and almost nobody issues it
# in a hot loop) while burying MSET and HSET. Sorting on size alone is sorting on one axis of
# two. Curated by hand and deliberately short: these are the commands that appear in
# redis-benchmark's default set or in ordinary client hot paths.
HOT_COMMANDS = {
    "get", "set", "mset", "mget", "incr", "decr", "hset", "hget", "hgetall", "del",
    "exists", "expire", "lpush", "rpush", "lpop", "rpop", "sadd", "srem", "sismember",
    "zadd", "zscore", "zrange", "scan", "ttl", "type", "setex", "getset", "append",
}


def cascade_positions(floor_src: str) -> dict[str, int]:
    """First cascade arm position per parser stem, in dispatch order."""
    out: dict[str, int] = {}
    n = 0
    for i, line in enumerate(floor_src.split("\n")):
        m = re.search(r"parse_borrowed_plain_([a-z0-9_]+)_packet\(unparsed", line)
        if m and 6000 < i < 14000:
            n += 1
            out.setdefault(m.group(1), n)
    return out


def real_commands(cmd_src: str) -> set[str]:
    start = cmd_src.index("const COMMAND_TABLE:")
    body = cmd_src[start : cmd_src.index("];", start)]
    return {m.group(1).lower() for m in re.finditer(r'^\s*\("([a-z0-9_|-]+)"\s*,', body, re.M)}


def report(show_all: bool) -> int:
    floor_src = open(FLOOR).read()
    stems = executor_stems(open(RUNTIME).read())
    done = floor_classified(floor_src)
    pos = cascade_positions(floor_src)
    cmds = real_commands(open(CMD_TABLE).read())

    rows = []
    for c in sorted(cmds - done):
        hits = has_executor(c, stems)
        if not hits:
            continue
        arm = pos.get(c)
        rows.append((arm if arm else 0, c, hits, arm))

    rows.sort(reverse=True)
    print(f"{len(stems)} executor stems; {len(done)} commands floor-classified\n")
    print(f"{'command':<14}{'arm':>5}{'pred dispatch':>15}  verdict")
    worth = []
    for _, c, hits, arm in rows:
        hot = " HOT" if c in HOT_COMMANDS else ""
        measured = MEASURED_DISPATCH.get(c)
        if c in NOT_SERVABLE:
            verdict = "admission guard refuses / container"
        elif arm is None:
            verdict = "no own arm (shared parser or non-cascade)"
        elif measured is not None:
            # A measurement always beats the fit. Compare against the band this arity would
            # land in rather than against a law-derived arm threshold.
            band = FRONT_CLASSIFIED_FLOOR_MIN
            if measured > band * 1.3:
                verdict = f"CANDIDATE — MEASURED {measured:.0f} vs band ~{band}{hot}"
                worth.append((arm, c))
            else:
                verdict = f"measured {measured:.0f}, inside band ~{band}{hot}"
        elif arm >= WORTH_IT:
            verdict = f"CANDIDATE — predicted deep{hot}"
            worth.append((arm, c))
        else:
            # (frankenredis-ozrro) A PREDICTION NEVER CLOSES A VEIN. This used to say
            # "marginal (below WORTH_IT)" for arms 10-29 and "already at the front" below 10,
            # both of which read as verdicts. They are not: the law under-predicts by ~50 pct
            # at exactly these depths, so an unmeasured shallow arm is UNSIZED, not cheap.
            verdict = f"UNSIZED — predicted {SLOPE * arm + INTERCEPT:.0f}, needs a shape{hot}"
            if c in HOT_COMMANDS:
                worth.append((arm, c))
        if not show_all and verdict.startswith("already"):
            continue
        pred = f"{SLOPE * arm + INTERCEPT:.0f}" if arm else "-"
        print(f"{c:<14}{arm if arm else '-':>5}{pred:>15}  {verdict}")

    print(f"\nCANDIDATES — measured above the floor, or HOT and unsized ({len(worth)}):")
    for arm, c in sorted(worth, reverse=True):
        print(f"  arm {arm:>3}  {c}")
    if not worth:
        print("  none measured above its band, and no hot command left UNSIZED.")
        print("  NOTE: that is not the same as 'exhausted'. This screen has produced two")
        print("  false exhausted verdicts already — once by missing parameterised classes,")
        print("  once by trusting a law-derived cutoff. Say 'nothing left to size', not")
        print("  'nothing left'.")
    return 0


def _self_test() -> int:
    # STEM matching is the whole point: an exact-name scan missed spop_count.
    stems = executor_stems(
        "fn execute_plain_spop_count_borrowed(\nfn execute_plain_get_borrowed_into(\n"
    )
    assert "spop_count" in stems, stems
    assert has_executor("spop", stems) == ["spop_count"], has_executor("spop", stems)
    # and it must not match a DIFFERENT command that merely shares a prefix
    assert has_executor("spo", stems) == [], has_executor("spo", stems)
    assert has_executor("get", stems) == ["get"], has_executor("get", stems)

    # Depth must be reported, because "executor exists" alone is not a worklist.
    src = "\n".join(
        ["x"] * 6001
        + [
            "  if let Some(p) = parse_borrowed_plain_alpha_packet(unparsed, &cfg) {",
            "  } else if let Some(p) = parse_borrowed_plain_beta_packet(unparsed, &cfg) {",
        ]
    )
    pos = cascade_positions(src)
    assert pos == {"alpha": 1, "beta": 2}, pos

    # A command enum reference is classification; a CLASS reference is not (GeohashSingle).
    assert floor_classified("BorrowedDispatchFloorCommand::Spop,") == {"spop"}
    assert floor_classified("BorrowedDispatchFloorClass::GeohashSingle") == set()

    assert "xread" in NOT_SERVABLE, "stream commands are not servable by a floor entry"
    assert WORTH_IT == 30, "threshold must match cascade_depth.py"
    print("self-test ok")
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        raise SystemExit(_self_test())
    raise SystemExit(report("--all" in sys.argv))
