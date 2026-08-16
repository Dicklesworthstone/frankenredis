#!/usr/bin/env python3
"""Find floor classes that CANNOT keep their promise, using the incumbent as oracle.

A floor class is a promise that its arm can serve the shape (frankenredis-opmo4).
Where the class is minted from ARITY ALONE but the arm's parser discriminates on a
KEYWORD, the promise is unchecked until the parser runs -- and by then the claim is
made, so the decline falls through to the GENERIC dispatcher rather than back to the
cascade. Measured instances: ZRANGE REV at 0.8658 against its accepted sibling's
1.16-1.22 (frankenredis-jnf09), LPOS COUNT at 2920.5 dispatch against its base's 466.9
(frankenredis-2e4tq), MGET 2-8 keys (opmo4), PFADD/LPUSHX/RPUSHX (dzik2).

Those four were each found by hand, one at a time. This screens the class instead.

THE ORACLE IS NOT OUR OWN SOURCE, deliberately. Reading fr's parsers to decide which
shapes exist would be circular -- the bug IS that fr's view of a shape set is
incomplete, so a corpus derived from it inherits the same blind spot
(frankenredis-feedback_test_oracle_derived_from_source_is_tautological). Instead the
option set comes from the vendored Redis 7.2.4 command table in
legacy_redis_code/redis/src/commands/*.json, which is the definition of what a client
may send.

METHOD. For every command with a floor class minted at a fixed array length N, count
the DISTINCT optional token forms that also produce array length N. Two or more means
the class is ambiguous at N: at most one of them is what the arm's parser accepts, and
every other one is claimed and refused.

LIMITS, stated because a screen that overstates is worse than none. It reports
AMBIGUITY, not a confirmed defect: an arm may chain several parsers and serve every
form (HMGET does exactly this and is correct). It only models token-plus-value and
bare-flag options, so commands whose optionals are variadic or nested blocks are
reported as "unmodelled" rather than silently scored. Confirm a hit by reading the arm.

Usage:  python3 scripts/arity_misclaim_screen.py [--all]
"""

import glob
import json
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MAIN = os.path.join(REPO, "crates/fr-server/src/main.rs")
CMDS = os.path.join(REPO, "legacy_redis_code/redis/src/commands")

# (N, BorrowedDispatchFloorCommand::Cmd) => ... BorrowedDispatchFloorClass::Class
FIXED = re.compile(
    r"\((\d+),\s*BorrowedDispatchFloorCommand::(\w+)\)[^\n]*?"
    r"(?:=>\s*(?:\{\s*)?Some\(BorrowedDispatchFloorClass::(\w+))?",
)


def floor_claims():
    """Fixed-arity floor claims as (arity, command, class)."""
    src = open(MAIN, encoding="utf-8", errors="replace").read()
    start = src.index("fn classify_borrowed_dispatch_floor_packet_impl")
    body = src[start:src.index("\n}", start)]
    out = []
    for line_no, line in enumerate(body.splitlines()):
        m = re.match(r"\s*\((\d+),\s*BorrowedDispatchFloorCommand::(\w+)\)", line)
        if not m:
            continue
        arity, cmd = int(m.group(1)), m.group(2)
        # The class may be on this line or the next; take the first one seen.
        tail = "\n".join(body.splitlines()[line_no:line_no + 3])
        c = re.search(r"BorrowedDispatchFloorClass::(\w+)", tail)
        out.append((arity, cmd, c.group(1) if c else "?"))
    return out


def option_forms(cmd):
    """Distinct optional token forms from the incumbent, as {token: elements_added}.

    Returns None when the command's optionals are not modelled (variadic or nested),
    so the caller can report it rather than score it wrongly.
    """
    path = os.path.join(CMDS, cmd.lower() + ".json")
    if not os.path.exists(path):
        return None, None
    spec = json.load(open(path))
    spec = spec[list(spec)[0]]
    mandatory = 1  # the command name itself
    forms = {}

    def width(arg):
        """Array elements this argument occupies, or None if not modelled."""
        if arg.get("multiple"):
            return None
        kind = arg.get("type")
        if kind == "pure-token":
            return 1
        if kind == "block":
            # A block is its own token (if any) plus each child.
            total = 1 if arg.get("token") else 0
            for child in arg.get("arguments", []):
                w = width(child)
                if w is None:
                    return None
                total += w
            return total
        if kind == "oneof" or "arguments" in arg:
            return None  # handled by the caller, which splits it into alternatives
        return (2 if arg.get("token") else 1)

    for arg in spec.get("arguments", []):
        optional = arg.get("optional")
        kind = arg.get("type")

        # A oneof is a CHOICE: each alternative is a distinct form at its own width.
        # This is what makes ZRANGE modellable -- BYSCORE and BYLEX are alternatives
        # of one `sortby` argument, and each shares an array length with REV and
        # WITHSCORES.
        if kind == "oneof":
            if not optional:
                return None, None  # a mandatory choice shifts the base; not modelled
            for alt in arg.get("arguments", []):
                w = width(alt)
                if w is None:
                    return None, None
                forms[alt.get("token") or alt.get("name", "?").upper()] = w
            continue

        w = width(arg)
        if w is None:
            return None, None
        if not optional:
            mandatory += w
            continue
        forms[arg.get("token") or "<positional:%s>" % arg.get("name", "?")] = w
    return mandatory, forms


def self_test(ambiguous):
    """Require the screen to still catch the instances we already know about.

    Both anchors are AMBIGUITY facts about the incumbent's grammar, not about fr's
    current arms, so they stay true after the arms are fixed -- ZRANGE's arm now
    serves all four *5 forms (frankenredis-jnf09) and the class is still ambiguous.
    That is what makes them stable anchors rather than things that go green when
    someone fixes the bug.

    Without these, a regression in the oneof/block modelling would empty the
    AMBIGUOUS list and read as "no defects found".
    """
    flagged = {(cmd, arity): hits for arity, cmd, _cls, hits in ambiguous}
    bad = []
    for cmd, arity, want in (("Zrange", 5, {"REV", "BYSCORE", "BYLEX", "WITHSCORES"}),
                             ("Lpos", 5, {"RANK", "COUNT", "MAXLEN"})):
        got = flagged.get((cmd, arity))
        if got is None:
            bad.append("%s at arity %d is not flagged; the oneof/block modelling has "
                       "regressed and this screen is now blind to its own anchor"
                       % (cmd, arity))
        elif set(got) != want:
            bad.append("%s at arity %d flagged %s, expected %s"
                       % (cmd, arity, sorted(got), sorted(want)))
    for line in bad:
        print("SELF-TEST FAIL: " + line)
    if bad:
        return 1
    print("self-test: ZRANGE and LPOS anchors still flagged with the expected forms")
    return 0


def main():
    show_all = "--all" in sys.argv
    if not os.path.isdir(CMDS):
        sys.exit("no incumbent command table at %s" % CMDS)

    claims = floor_claims()
    ambiguous, clean, unmodelled = [], [], []
    for arity, cmd, cls in claims:
        mandatory, forms = option_forms(cmd)
        if forms is None:
            unmodelled.append((arity, cmd, cls))
            continue
        hits = [t for t, add in forms.items() if mandatory + add == arity]
        if len(hits) >= 2:
            ambiguous.append((arity, cmd, cls, hits))
        else:
            clean.append((arity, cmd, cls, hits))

    if "--self-test" in sys.argv:
        return self_test(ambiguous)

    print("AMBIGUOUS -- %d floor class(es) minted at an arity that several option "
          "forms share." % len(ambiguous))
    print("At most one is what the arm's parser accepts; confirm by reading the arm.\n")
    for arity, cmd, cls, hits in sorted(ambiguous):
        print("  %-10s arity %-2d -> %-22s  forms at this arity: %s"
              % (cmd, arity, cls, ", ".join(sorted(hits))))

    print("\nUNMODELLED -- %d claim(s) whose optionals are variadic or nested; this "
          "screen cannot score them and does not guess." % len(unmodelled))
    if show_all:
        for arity, cmd, cls in sorted(unmodelled):
            print("  %-10s arity %-2d -> %s" % (cmd, arity, cls))
        print("\nUNAMBIGUOUS -- %d claim(s)." % len(clean))
        for arity, cmd, cls, hits in sorted(clean):
            print("  %-10s arity %-2d -> %-22s  %s"
                  % (cmd, arity, cls, ", ".join(sorted(hits)) or "no option at this arity"))
    else:
        print("(re-run with --all to list unmodelled and unambiguous claims)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
