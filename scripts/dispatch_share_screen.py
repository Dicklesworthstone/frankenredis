#!/usr/bin/env python3
"""Rank every harness shape by ABSOLUTE dispatch instr/op — the front-classification
target list.

(frankenredis-z2ce3, frankenredis-ozrro) This existed only in a scratchpad while it
produced the screen banked in ledger row `211b3280e`, which is why it is here now: that
row's conclusion — that the front-classification surface is ~ONE command rather than a
campaign — is not reproducible without it.

WHY ABSOLUTE, NOT SHARE. Ranking by dispatch SHARE misleads: a 30% share of a small
command is worth less than an 8% share of a large one. `set_xx_opt` at 30.5% share is
986.9 instr/op while `sinter_9` at 8.3% is 1007.6. Sort by the instructions, then read
the share as context.

⚠ EVERY NUMBER BELOW WAS PRODUCED BY A BROKEN METRIC AND IS WITHDRAWN (frankenredis-7so0e).
Until this screen is re-run, treat the table as a list of SHAPES WORTH RE-MEASURING and not
as values. `dispatch_share()` used to evaluate the share on the 2N dump ALONE -- startup,
seeding and teardown included -- and multiply it by the clean two-point instructions/op; a
share of one population times a rate from another is not per-op. On `SORT_RO ... ALPHA` it
printed 2,048.3 / 2,517.9 / 2,535.3 / 3,358.5 across four arms whose true dispatch is 3,116.0
in every one, and it manufactured a 487 instr/op "reduction" out of a change that never
touched dispatch. Its frame list was ALSO missing `classify_borrowed_dispatch_floor_packet`
-- the floor classifier itself -- which under-counted CLASSIFIED routes by 58% against 7.6%
for generic ones, i.e. it exaggerated exactly the classified-vs-generic gap this screen
exists to rank. Four rows across two agents were withdrawn on it (`b9c288a1d`). The metric is
fixed as of that bead; these figures predate the fix.

AND THE OPENING ARGUMENT BELOW IS NOW MEASURED, NOT JUST REASONED. Recomputed two-point
(`cbc702a54`), the front-classified band is 16.8-37.1 pct and the generic band 33.9-43.2 pct
-- they OVERLAP, and `lmpop_missing` (classified) carries a HIGHER share than
`xpending_populated` (generic). **Dispatch share does not discriminate mechanism.** Absolute
instr/op still does, cleanly: classified routes 543-1,035, the generic pair 2,804-3,374, about
3x apart with no overlap. So rank on the ABSOLUTE column exactly as this docstring already
says, and do not use the share column to decide whether a route is stranded -- use the source
test (`borrowed_dispatch_floor_command` entry, parser arm, executor) or the mechanism label.

WHAT THE SCREEN FOUND (ELF 1b1d66cd, 102 shapes) -- WITHDRAWN, see above: only THREE shapes
pay more than 1,000 instr/op of dispatch, and two of them are the SORT shapes. 114 commands are already
floor-classified, so the tail is flat.

    3123.0  24.0%   12990.6   sort_ro_alpha
    3005.7   2.2%  138682.7   sort_ro_alpha_64
    1007.6   8.3%   12109.3   sinter_9
    ---------------------------------------- 1,000 instr/op
     986.9  30.5%    3237.7   set_xx_opt

KNOWN LIMIT, and it is the important one: this ranks the shapes the harness HAS. LTRIM,
SPOP, SRANDMEMBER, SMOVE, RPOPLPUSH, HSET and INCRBYFLOAT are unclassified AND have no
shape, so they are UNMEASURED rather than cheap — and the LTRIM cascade walk is recorded
elsewhere at 15,736 instr/op, 5x anything in the table. The corpus hides its own worst
case. Add shapes for those before planning front-classification work off this ranking.

The ratio column is a single draw per shape and carries the redis-side elapsed-time
contaminant (see the `min-of-K` discussion in shape_instr_per_op.py's docstring); treat it
as directional. The dispatch column is fr-side and is the number this tool exists for.

Usage:
    dispatch_share_screen.py <fr_bin> [--top N]
    dispatch_share_screen.py --self-test
"""

from __future__ import annotations

import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HARNESS = os.path.join(ROOT, "scripts", "shape_instr_per_op.py")

# One parsed row: (dispatch_instr, dispatch_share_pct, instr_per_op, ratio_or_nan, shape).
_IPO = re.compile(r"^\s+fr\s+.*->\s+([\d.]+) instr/op", re.M)
_DISP = re.compile(r"dispatch share:\s+([\d.]+)%\s+\(~([\d.]+)")
_RATIO = re.compile(r"instructions per op:\s+([\d.]+)x")


def parse_shape_output(out: str):
    """Pull (dispatch_instr, share_pct, instr_per_op, ratio) out of one harness run.

    Returns None when the run did not produce a usable fr arm — a shape that errored
    must be DROPPED from the ranking, never silently scored as zero, which would put it
    at the bottom of a list whose whole purpose is to find the top.
    """
    ipo, disp = _IPO.search(out), _DISP.search(out)
    if not (ipo and disp):
        return None
    ratio = _RATIO.search(out)
    return (
        float(disp.group(2)),
        float(disp.group(1)),
        float(ipo.group(1)),
        float(ratio.group(1)) if ratio else float("nan"),
    )


def shape_names() -> list[str]:
    listing = subprocess.run(
        [sys.executable, HARNESS, "--list"], capture_output=True, text=True
    ).stdout
    names = [s.strip() for s in re.sub(r"^shapes:", "", listing).split(",")]
    return [s for s in names if re.fullmatch(r"[a-z0-9_]+", s)]


def _self_test() -> int:
    """Parser check against captured harness output, including the shapes that broke it.

    The first screen I ran mis-parsed the shape list and scored 102 shapes as blanks; the
    ranking looked plausible and was empty. That is the failure this guards.
    """
    good = (
        "shape get_control   N=2000 2N=4000\n"
        "  fr     Ir(N)=3726044        Ir(2N)=6396737        ->     1335.3 instr/op\n"
        "  redis  Ir(N)=13583991       Ir(2N)=19988144       ->     3202.1 instr/op\n"
        "  fr/redis instructions per op: 0.4170x\n"
        "  fr dispatch share: 20.6%  (~275.0 of 1335.3 instr/op deciding WHICH command)\n"
    )
    got = parse_shape_output(good)
    assert got is not None, "well-formed output must parse"
    disp, share, ipo, ratio = got
    assert abs(disp - 275.0) < 1e-6, disp
    assert abs(share - 20.6) < 1e-6, share
    assert abs(ipo - 1335.3) < 1e-6, ipo
    assert abs(ratio - 0.4170) < 1e-6, ratio

    # A run with no ratio line (shape measured on the fr arm only) still ranks.
    no_ratio = "\n".join(l for l in good.splitlines() if "instructions per op" not in l)
    got = parse_shape_output(no_ratio)
    assert got is not None and got[0] == 275.0, got
    assert got[3] != got[3], "missing ratio must be NaN, not 0.0"

    # A failed/empty run must be DROPPED, not scored zero.
    assert parse_shape_output("") is None
    assert parse_shape_output("shape bogus\nerror: no such shape\n") is None
    # fr arm present but no dispatch line is also unusable.
    assert parse_shape_output(good.replace("fr dispatch share", "x")) is None

    print("self-test ok")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return _self_test()
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    elf = sys.argv[1]
    top = 25
    if "--top" in sys.argv:
        top = int(sys.argv[sys.argv.index("--top") + 1])

    rows, dropped = [], []
    for shape in shape_names():
        out = subprocess.run(
            [sys.executable, HARNESS, elf, shape], capture_output=True, text=True
        ).stdout
        parsed = parse_shape_output(out)
        if parsed is None:
            dropped.append(shape)
            continue
        rows.append((*parsed, shape))

    rows.sort(reverse=True)
    print(f"{'DISPATCH':>9} {'share':>7} {'instr/op':>10} {'ratio':>8}  shape")
    for disp, share, ipo, ratio, shape in rows[:top]:
        print(f"{disp:9.1f} {share:6.1f}% {ipo:10.1f} {ratio:8.4f}  {shape}")

    print(f"\n{len(rows)} shapes measured")
    if dropped:
        # Never let a shape vanish silently: a dropped shape is unmeasured, not cheap.
        print(f"{len(dropped)} DROPPED (no usable fr arm): {', '.join(dropped)}")
    heavy = [r for r in rows if r[0] > 1000]
    print(
        f"{len(heavy)} shapes pay >1000 instr/op of dispatch; "
        f"recoverable if all reached a front-classified ~275: "
        f"{sum(r[0] - 275 for r in heavy):.0f} instr/op"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
