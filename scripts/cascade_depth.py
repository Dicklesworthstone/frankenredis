#!/usr/bin/env python3
"""Rank borrowed-cascade arms by DEPTH, which predicts their dispatch cost.

(frankenredis-ozrro) Two tools already answer "is this command floor-classified?" and
"does it have a borrowed fast path?". Neither answers the question that decides whether a
floor entry is worth building: HOW DEEP the command sits in the cascade. That is the whole
lever, and ignoring it put PING — arm 1 of 163, one failed parse deep — on my own list of
four commands to work next.

THE LAW, fitted over six commands each measured independently in its own ABBA row
(ledger `15bf17cb9`):

    command      arm   measured dispatch   per position
    smove        103          4,438            43.1
    hincrby      102          4,343            42.6
    zlexcount    101          4,305            42.6
    renamenx      99          4,123            41.6
    substr        78          3,378            43.3
    rpoplpush     76          3,069            40.4

    dispatch ~= 45.1 x arm_position - 261

The per-position figure varies only 40.4-43.3 across six commands with different arities,
parsers and executors, so depth does essentially all the work. 45.1 also independently
rederives this ledger's "one failed non-inlined parser call costs 40-50 instr/op", measured
by an unrelated experiment (moving a single SET arm and watching the jumped-over arms pay).

SCOPE LIMIT — READ THIS BEFORE CONCLUDING "NO CANDIDATES LEFT". This tool ranks arms
that are IN the cascade. A command with no borrowed parser has NO ARM, so it is invisible
here — and those are exactly the commands on the GENERIC route, the most expensive one
there is. An empty candidate list means "no unclassified command sits DEEP IN THE CASCADE",
never "no command is on an expensive route".

I read it the second way and reported the vein closed. Six commands invisible to this tool
were then measured cold and three came back ABOVE PARITY: scan 1.2810x, lcs 1.1115x,
keys 1.0269x, each carrying ~2,000-2,400 instr/op of dispatch (ledger, this bead). This is
the same shape of blind spot as `shared_executor_map.py` documents for `corpus_coverage.py`:
a tool that ranks positions WITHIN a structure cannot see what never enters it. Use
`corpus_coverage.py`'s [C] list for those; use this tool only to rank what is already an arm.

HONEST LIMIT, because the fit is used to rank and not to certify: all six points lie between
arms 76 and 103. The SLOPE is well constrained; the INTERCEPT is not, and extrapolating to
arm 1 returns a negative number, which is meaningless. Predictions below arm ~30 should be
read as "small" rather than as a figure, and every prediction is a prediction — the ABBA
that follows is what settles it.

Usage:
    cascade_depth.py [--top N]     # deepest arms first
    cascade_depth.py --self-test
"""

from __future__ import annotations

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FLOOR = os.path.join(ROOT, "crates", "fr-server", "src", "main.rs")
CMD_TABLE = os.path.join(ROOT, "crates", "fr-command", "src", "lib.rs")

SLOPE, INTERCEPT = 45.1, -261.0
# Below this the entry costs more review than it returns; see the ledger row.
WORTH_IT = 30

# Container commands dispatch their own name but do their work in SUBCOMMANDS, so a floor
# entry keyed on the bare name serves a packet that carries no real work. Excluded here for
# the same reason `corpus_coverage.py` excludes them, and listed rather than filtered
# silently — PUBSUB sits at arm 127 and would otherwise head this report.
CONTAINERS = {
    "acl", "client", "cluster", "command", "config", "debug", "function", "latency",
    "memory", "object", "pubsub", "script", "slowlog", "xgroup", "xinfo",
}


def cascade_arms(text: str) -> list[tuple[int, str]]:
    """(position, command) for each borrowed-cascade arm, in dispatch order.

    Positions come from the ORDER parser calls appear inside the cascade, which is the
    order a packet actually tries them — not from line numbers, which drift as unrelated
    code lands above.
    """
    lines = text.split("\n")
    out: list[tuple[int, str]] = []
    for i, line in enumerate(lines):
        m = re.search(r"parse_borrowed_plain_([a-z0-9_]+)_packet\(unparsed", line)
        if m and 6000 < i < 14000:
            out.append((len(out) + 1, m.group(1)))
    return out


def classified(text: str) -> set[str]:
    """Commands that already have a floor CLASS — the definitive signal.

    Keyed on the COMMAND enum, not the CLASS enum, and that distinction cost me a false
    candidate list. A class is named for the SHAPE it serves, not the command: GEOHASH's
    class is `GeohashSingle`, and HKEYS/HVALS map to shared classes under other names
    again. Matching `BorrowedDispatchFloorClass::<Cmd>` therefore reports all three as
    unclassified when the floor reaches them perfectly well.

    `BorrowedDispatchFloorCommand::<Cmd>` IS named after the command — it is the variant
    the token table produces — so it is the signal that survives. This is the fourth
    name-based check in this repo to be wrong in a NEW way; the pattern is that only the
    enum whose variants are DEFINED by command name can be trusted.
    """
    return {
        m.group(1).lower()
        for m in re.finditer(r"BorrowedDispatchFloorCommand::([A-Z][A-Za-z0-9]*)", text)
    }


def real_commands(text: str) -> set[str]:
    """Every name in fr-command's COMMAND_TABLE.

    Needed because a cascade arm is named after its PARSER, not a command, and the two
    diverge badly at depth: `exists_two` .. `exists_eight` are exact-N parsers all serving
    EXISTS, and `keyed_values9` .. `keyed_values14` serve a whole family. Reporting those
    as unclassified candidates inflated the candidate count from single digits to 73 in the
    first version of this tool — a false positive of exactly the kind the other two tools
    here have already produced, so it is checked rather than assumed.
    """
    start = text.index("const COMMAND_TABLE:")
    body = text[start : text.index("];", start)]
    return {m.group(1).lower() for m in re.finditer(r'^\s*\("([a-z0-9_|-]+)"\s*,', body, re.M)}


def predict(position: int) -> float:
    return SLOPE * position + INTERCEPT


def report(top: int) -> int:
    text = open(FLOOR).read()
    arms = cascade_arms(text)
    done = classified(text)
    cmds = real_commands(open(CMD_TABLE).read())

    def status(cmd: str) -> str:
        if cmd in CONTAINERS:
            return "container (work is in subcommands)"
        if cmd not in cmds:
            return "generic parser (serves a family)"
        return "classified" if cmd in done else "UNCLASSIFIED — CANDIDATE"

    rows = sorted(((p, c, predict(p), status(c)) for p, c in arms), key=lambda r: -r[0])
    cands = [r for r in rows if r[3].startswith("UNCLASS") and r[0] >= WORTH_IT]

    print(f"{len(arms)} cascade arms; {len(done)} commands already floor-classified\n")
    print(f"{'arm':>4}  {'parser stem':<22}{'predicted dispatch':>19}  status")
    for pos, cmd, pred, st in rows[:top]:
        print(f"{pos:>4}  {cmd:<22}{pred:>19.0f}  {st}")

    print(f"\nCANDIDATES — a real command, unclassified, at depth >= {WORTH_IT} ({len(cands)}):")
    for pos, cmd, pred, _ in cands:
        print(f"  arm {pos:>3}  {cmd:<20} predicted ~{pred:,.0f} instr/op of dispatch")
    if not cands:
        print("  none — every deep arm is either already classified or a family parser.")
    return 0


def _self_test() -> int:
    src = "\n".join(
        ["x"] * 6001
        + [
            "  if let Some(p) = parse_borrowed_plain_alpha_packet(unparsed, &cfg) {",
            "  } else if let Some(p) = parse_borrowed_plain_beta_packet(unparsed, &cfg) {",
            "  BorrowedDispatchFloorCommand::Beta,",
        ]
    )
    arms = cascade_arms(src)
    assert arms == [(1, "alpha"), (2, "beta")], arms
    # Position is ORDER, not line number — an arm that moves down the file without other
    # arms appearing above it keeps its position, which is what makes the law stable as
    # unrelated code lands.
    assert arms[0][0] == 1 and arms[1][0] == 2

    done = classified(src)
    assert done == {"beta"}, done
    # A CLASS reference must NOT be read as classification. GEOHASH's class is
    # `GeohashSingle`, so a class-keyed scan calls it unclassified when it is not — that
    # false positive is why this is keyed on the command enum.
    assert classified("BorrowedDispatchFloorClass::GeohashSingle") == set()
    assert classified("BorrowedDispatchFloorCommand::Geohash,") == {"geohash"}
    # A parser call OUTSIDE the cascade window must not count as an arm; counting one would
    # shift every position below it and silently corrupt every prediction.
    outside = "parse_borrowed_plain_gamma_packet(unparsed, &cfg)"
    assert cascade_arms(outside) == [], cascade_arms(outside)

    # A parser STEM that is not a real command must never be reported as a candidate.
    # exists_eight serves EXISTS (classified); calling it unclassified is the false
    # positive that inflated this tool's first candidate count to 73.
    tbl = 'const COMMAND_TABLE: &[(&str, i64)] = &[\n    ("exists", -2),\n    ("dump", 2),\n];\n'
    cmds = real_commands(tbl)
    assert cmds == {"exists", "dump"}, cmds
    assert "exists_eight" not in cmds and "keyed_values9" not in cmds

    assert "pubsub" in CONTAINERS, "a container must never be reported as a candidate"

    # The fit must reproduce the measured points it was built from, within their spread.
    for pos, measured in [(103, 4438), (101, 4305), (76, 3069)]:
        pred = predict(pos)
        assert abs(pred - measured) / measured < 0.06, (pos, pred, measured)

    print("self-test ok")
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        raise SystemExit(_self_test())
    n = int(sys.argv[sys.argv.index("--top") + 1]) if "--top" in sys.argv else 20
    raise SystemExit(report(n))
