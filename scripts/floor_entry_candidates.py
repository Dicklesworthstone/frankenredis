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

WHAT IT FOUND when written (fr-server at c7572f3a7): ten stem-matched candidates, and NONE
clears the bar. get is at arm 4 and ping at arm 1 — already at the front, so a floor entry
buys nothing. hset (16) and mset (19) sit below the WORTH_IT threshold `cascade_depth.py`
documents. move, spublish and bitfield_ro have no arm of their own. The remainder are stream
and pubsub commands, which the borrowed admission guard refuses anyway. Class [A] is
EXHAUSTED for anything worth doing — established by ranking, not by assertion.

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
    """Commands with a floor entry, keyed on the COMMAND enum (named after the command)."""
    return {
        m.group(1).lower()
        for m in re.finditer(
            r"BorrowedDispatchFloorCommand::([A-Z][A-Za-z0-9]*)", floor_src
        )
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
        if c in NOT_SERVABLE:
            verdict = "admission guard refuses / container"
        elif arm is None:
            verdict = "no own arm (shared parser or non-cascade)"
        elif arm >= WORTH_IT:
            verdict = "CANDIDATE"
            worth.append((arm, c))
        elif arm >= 10:
            verdict = f"marginal (below WORTH_IT={WORTH_IT})"
        else:
            verdict = "already at the front of the cascade"
        if not show_all and verdict.startswith("already"):
            continue
        pred = f"{SLOPE * arm + INTERCEPT:.0f}" if arm else "-"
        print(f"{c:<14}{arm if arm else '-':>5}{pred:>15}  {verdict}")

    print(f"\nCANDIDATES at depth >= {WORTH_IT} ({len(worth)}):")
    for arm, c in sorted(worth, reverse=True):
        print(f"  arm {arm:>3}  {c}")
    if not worth:
        print("  none — class [A] is exhausted for anything worth doing.")
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
