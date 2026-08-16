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
# Overridable so the screen can be pointed at a HISTORICAL main.rs and validated
# against a tree where the defect is known to exist. Without that, a run reporting
# zero gaps is indistinguishable from a screen that cannot find any.
MAIN = os.environ.get("FR_MAIN_RS") or os.path.join(REPO, "crates/fr-server/src/main.rs")
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


PREFIX = re.compile(r'b"\*(\d+)\\r\\n')

# name -> minimum array length a length-flexible parser accepts (None = unknown)
MINLEN: dict = {}
# name -> MAXIMUM array length it accepts. A flexible parser has BOTH bounds, and
# checking only the lower one misses the mis-claim at the TOP of the range: a class
# minted `arity >= 2` against a parser refusing `nkeys > 32` claims every 33-key call
# it cannot serve (frankenredis-9hnxt).
MAXLEN: dict = {}


def form_map(cmd):
    """Before you claim an arity for a command, ask what ELSE has that length.

    The screens above only inspect classes that EXIST. This answers the question
    that comes first and is where the family's near-misses happen: dzik2 caught
    PFADD one step before shipping, and GEOADD had to be worked out by hand
    (frankenredis-wjzs9). It handles the variadic case the other modes park as
    unmodelled, by solving for the repeat count rather than enumerating widths.

    Array length = base + (sum of chosen optional widths) + W*n, for a command
    with one variadic block of width W repeated n>=1 times. For each length we
    report every option-subset admitting an integer n, so two or more means a
    class minted there cannot know which form it has.
    """
    path = os.path.join(CMDS, cmd.lower() + ".json")
    if not os.path.exists(path):
        return None
    spec = json.load(open(path))
    spec = spec[list(spec)[0]]

    base = 1              # the command name
    opts = []             # (label, width) for each independent optional
    var_width = None      # width of the single variadic block, if any

    def fixed_width(arg):
        if arg.get("type") == "pure-token":
            return 1
        if arg.get("type") == "block":
            w = 1 if arg.get("token") else 0
            for c in arg.get("arguments", []):
                cw = fixed_width(c)
                if cw is None:
                    return None
                w += cw
            return w
        if arg.get("type") == "oneof" or "arguments" in arg:
            return None
        return 2 if arg.get("token") else 1

    for arg in spec.get("arguments", []):
        w = fixed_width(arg)
        if arg.get("multiple"):
            if var_width is not None or w is None:
                return None
            var_width = w
            continue
        if arg.get("type") == "oneof":
            alts = []
            for alt in arg.get("arguments", []):
                aw = fixed_width(alt)
                if aw is None:
                    return None
                alts.append((alt.get("token") or alt.get("name", "?").upper(), aw))
            if arg.get("optional"):
                opts.append(alts)          # choose one, or none
            else:
                base += alts[0][1]
            continue
        if w is None:
            return None
        if arg.get("optional"):
            opts.append([(arg.get("token") or arg.get("name", "?").upper(), w)])
        else:
            base += w

    # Every combination of "take one alternative of this optional, or skip it".
    combos = [("", 0)]
    for alts in opts:
        nxt = []
        for label, width in combos:
            nxt.append((label, width))                     # skip
            for tok, w in alts:
                nxt.append((" ".join(x for x in [label, tok] if x), width + w))
        combos = nxt

    forms = {}
    for label, width in combos:
        if var_width:
            for n in range(1, 6):
                forms.setdefault(base + width + var_width * n, []).append(
                    "%s x%d" % (label or "(plain)", n))
        else:
            forms.setdefault(base + width, []).append(label or "(plain)")
    return base, var_width, forms


def advise(cmd):
    got = form_map(cmd)
    if got is None or isinstance(got, int):
        return 1
    base, var_width, forms = got
    print("%s: base=%d, variadic block width=%s" % (cmd, base, var_width))
    for length in sorted(forms):
        if length > base + 14:
            break
        tag = "AMBIGUOUS" if len(forms[length]) > 1 else "unique   "
        print("  array_len %2d  %s  %s" % (length, tag, " | ".join(forms[length])))
    print("\nOnly `unique` lengths are safe to mint a class at without a keyword check.")
    print("AMBIGUOUS means ambiguous TO A CLASSIFIER THAT SEES ONLY THE ARRAY LENGTH.")
    print("A command carrying an explicit count -- LMPOP's `numkeys`, ZADD's pairing --")
    print("can still be resolved, by READING that count at classification time rather")
    print("than inferring it from the length. That costs a parse in the classifier, so")
    print("it is a trade, not a free out; but do not read these rows as `impossible`.")
    return 0


def sweep():
    """Every fixed-arity class, checked against the variadic solver.

    The keyword mode enumerates fixed argument widths and parks anything variadic,
    which left 30 fixed-arity claims unexamined. form_map solves for the repeat
    count instead, so those can be checked too -- 13 remain unmodelled here rather
    than 30.
    """
    ambiguous, clean, unmodelled = [], 0, 0
    for arity, cmd, cls in floor_claims():
        got = form_map(cmd)
        if got is None:
            unmodelled += 1
            continue
        _base, _w, forms = got
        hits = forms.get(arity, [])
        if len(hits) >= 2:
            ambiguous.append((cmd, arity, cls, tuple(hits)))
        else:
            clean += 1
    print("AMBIGUOUS AT THEIR CLAIMED ARITY -- %d" % len(set(ambiguous)))
    for cmd, arity, cls, hits in sorted(set(ambiguous)):
        print("  %-12s arity %-2d -> %-22s %s"
              % (cmd, arity, cls, " | ".join(hits)))
    print("\nunambiguous: %d   still unmodelled: %d" % (clean, unmodelled))
    print("Each hit needs the ARM read: an arm may chain parsers and serve every")
    print("form (GETEX does), and a hit is ambiguity, never a confirmed defect.")
    return 0


def const_values():
    """`const NAME: usize = N;` from the same file, so caps are read not guessed."""
    src = open(MAIN, encoding="utf-8", errors="replace").read()
    return {m.group(1): int(m.group(2))
            for m in re.finditer(r"const ([A-Z_]+): usize = (\d+);", src)}


CONSTS: dict = {}


def parser_prefixes():
    CONSTS.update(const_values())
    """Fixed array length each named parser pins internally, when it pins one."""
    src = open(MAIN, encoding="utf-8", errors="replace").read()
    out = {}
    for m in re.finditer(r"\nfn (parse_borrowed_\w+)", src):
        name = m.group(1)
        # Bound the body at the next top-level `fn`, NOT by a fixed window. A fixed
        # window spilled into the following function and pinned
        # parse_borrowed_plain_keys_multi_packet -- which reads its length
        # dynamically and serves 10.. -- to a neighbour's literal `*9`, reporting
        # SINTER/SUNION/SDIFF as range gaps they do not have.
        nxt = src.find("\nfn ", m.end())
        body = src[m.end():nxt if nxt != -1 else len(src)]
        hits = {int(h) for h in PREFIX.findall(body)}
        # A parser that derives the length at runtime pins no FIXED length -- but it
        # usually still has a LOWER BOUND, and ignoring that is what made the first
        # version of this mode vacuous: it reported zero gaps on the very tree where
        # MGET was known broken, because keys_multi looked merely "flexible" when in
        # fact it refuses anything under 10.
        if 'strip_prefix(b"*")' in body:
            lo = re.search(r"if arr_len < (\d+)", body)
            MINLEN[name] = int(lo.group(1)) if lo else None
            # Upper bound. The guard counts KEYS/FIELDS/MEMBERS, not array
            # elements, so convert with the parser's own `let nX = arr_len - K`.
            hi = re.search(r"n\w+ > ([A-Z_]+)", body)
            off = re.search(r"let n\w+ = arr_len - (\d+);", body)
            if hi and off:
                cap = CONSTS.get(hi.group(1))
                MAXLEN[name] = None if cap is None else cap + int(off.group(1))
            else:
                MAXLEN[name] = None
            out[name] = None
            continue
        # One fixed prefix means the parser serves exactly that length. Several (or
        # none) means it is length-flexible and this screen cannot bound it.
        out[name] = hits.pop() if len(hits) == 1 else None
    return out


def arm_arities(prefixes):
    """Array lengths each class arm can serve, or None when not boundable.

    An arm pins a length two ways: a literal `b"*N\\r\\n"` passed to a generic parser
    at the call site, or a named parser that pins one internally. If any parser it
    calls is length-flexible, the arm is unbounded and we say so rather than guess.
    """
    src = open(MAIN, encoding="utf-8", errors="replace").read()
    start = src.index("fn try_dispatch_floor_classified_action")
    body = src[start:]
    arms, order = {}, []
    for m in re.finditer(r"\n        BorrowedDispatchFloorClass::(\w+)", body):
        order.append((m.group(1), m.start()))
    for i, (cls, pos) in enumerate(order):
        end = order[i + 1][1] if i + 1 < len(order) else min(pos + 4000, len(body))
        chunk = body[pos:end]
        served = {int(h) for h in PREFIX.findall(chunk)}
        flexible = False
        minima = []
        maxima = []
        for called in re.findall(r"(parse_borrowed_\w+)\(", chunk):
            pinned = prefixes.get(called)
            if pinned is None:
                if called == "parse_borrowed_multibulk_action":
                    continue
                lo = MINLEN.get(called)
                if lo is None:
                    flexible = True          # genuinely unbounded: do not guess
                else:
                    minima.append(lo)
                    maxima.append(MAXLEN.get(called))
            else:
                served.add(pinned)
        # A flexible parser with a floor serves that floor and everything above it,
        # so record it as coverage from `lo` upward rather than as an unknown.
        arms.setdefault(cls, (set(), False, None, None))
        prev, prev_flex, prev_min, prev_max = arms[cls]
        newmin = min([m for m in minima + [prev_min] if m is not None], default=None)
        # An unknown ceiling on ANY parser means the arm's top is unknown.
        allmax = maxima + ([prev_max] if prev_max is not None else [])
        newmax = None if (not allmax or any(m is None for m in maxima)) else max(allmax)
        arms[cls] = (prev | served, prev_flex or flexible, newmin, newmax)
    return arms


RANGE_MINT = re.compile(
    r"\((?:(\d+)\.\.=(\d+)|arity|array_len),\s*BorrowedDispatchFloorCommand::(\w+)\)"
    r"(?:\s*if\s*\(?(\d+)\.\.=(\d+)\)?\.contains|\s*if\s*\w+\s*>=\s*(\d+))?"
)


def range_claims():
    """Classes minted over a RANGE of arities, as (cmd, lo, hi_or_None, class)."""
    src = open(MAIN, encoding="utf-8", errors="replace").read()
    start = src.index("fn classify_borrowed_dispatch_floor_packet_impl")
    body = src[start:src.index("\n}", start)]
    out = []
    for line in body.splitlines():
        m = RANGE_MINT.search(line)
        if not m:
            continue
        lo = m.group(1) or m.group(4) or m.group(6)
        hi = m.group(2) or m.group(5)
        if lo is None:
            continue
        idx = body.index(line)
        cls = re.search(r"BorrowedDispatchFloorClass::(\w+)", body[idx:idx + 400])
        out.append((m.group(3), int(lo), int(hi) if hi else None,
                    cls.group(1) if cls else "?"))
    return out


def report_ranges():
    prefixes = parser_prefixes()
    arms = arm_arities(prefixes)
    gaps, unbounded = [], []
    for cmd, lo, hi, cls in range_claims():
        served, flexible, floor, ceiling = arms.get(cls, (set(), True, None, None))
        if flexible or (not served and floor is None):
            unbounded.append((cmd, lo, hi, cls))
            continue
        top = hi if hi is not None else max(list(served) + [floor or lo])
        missing = [a for a in range(lo, top + 1)
                   if a not in served and not (floor is not None and a >= floor)]
        # TOP OF THE RANGE: an open-ended class against a capped parser claims
        # everything above the cap and serves none of it.
        if hi is None and ceiling is not None:
            missing.append("%d+ (parser caps at %d)" % (ceiling + 1, ceiling))
        if missing:
            gaps.append((cmd, lo, hi, cls, missing, sorted(served)))

    print("RANGE GAPS -- %d class(es) minted over arities their arm cannot serve.\n"
          "This is the MGET/PFADD sub-species: the class promises a range, the arm's\n"
          "parsers pin specific lengths, and the arities in between fall to generic.\n"
          % len(gaps))
    for cmd, lo, hi, cls, missing, served in sorted(gaps):
        rng = "%d..=%d" % (lo, hi) if hi else "%d.." % lo
        print("  %-12s claims %-8s -> %-22s arm serves %s, UNSERVED %s"
              % (cmd, rng, cls, served, missing))
    print("\nUNBOUNDED -- %d class(es) whose arm calls a length-flexible parser; this\n"
          "screen cannot bound them and does not guess." % len(unbounded))
    return 0


def self_test_ranges():
    """Pin the parser facts that make --range non-vacuous.

    The first version of --range reported ZERO gaps on 645845b0e, the tree where
    MGET was known broken, because it treated a length-flexible parser as
    unbounded and skipped it. keys_multi is not unbounded -- it refuses anything
    under 10 -- and that floor is the whole defect. If these stop being read, the
    mode silently goes quiet instead of going red.
    """
    prefixes = parser_prefixes()
    bad = []
    # FLOORS are being actively tuned (9hnxt moved keys_multi's, xqqwv moved
    # hmget/zmscore's), so anchoring them to exact values would make this fail on
    # every legitimate change and train people to ignore it. What must not regress
    # is that a floor is READ AT ALL -- that was the vacuity, not the value.
    for name in ("parse_borrowed_plain_keys_multi_packet",
                 "parse_borrowed_plain_hmget_multi_packet"):
        if MINLEN.get(name) is None:
            bad.append("%s floor is unreadable; a flexible parser with no floor is "
                       "treated as unbounded and skipped, which is exactly how this "
                       "mode reported zero on a known-broken tree" % name)

    # CEILINGS are structural -- they come from `const *_MULTI_MAX` and each
    # parser's own `let nX = arr_len - K` offset, which differ (keys_multi -1,
    # hmget_multi -2). Reading the constant without the offset gives the wrong
    # array length, so these are pinned exactly.
    for name, want in (("parse_borrowed_plain_keys_multi_packet", 33),
                       ("parse_borrowed_plain_hmget_multi_packet", 34)):
        if MAXLEN.get(name) != want:
            bad.append("%s ceiling read as %r, expected array length %d"
                       % (name, MAXLEN.get(name), want))

    if prefixes.get("parse_borrowed_plain_hmget2_packet") != 4:
        bad.append("hmget2 fixed prefix read as %r, expected 4"
                   % prefixes.get("parse_borrowed_plain_hmget2_packet"))

    for line in bad:
        print("SELF-TEST FAIL: " + line)
    if bad:
        return 1
    print("self-test: floors readable, ceilings exact (keys_multi..33, "
          "hmget_multi..34), hmget2 prefix==4")
    return 0


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

    if "--sweep" in sys.argv:
        return sweep()

    if "--advise" in sys.argv:
        i = sys.argv.index("--advise")
        if i + 1 >= len(sys.argv):
            sys.exit("--advise needs a command name, e.g. --advise GEOADD")
        return advise(sys.argv[i + 1])

    if "--range" in sys.argv:
        return report_ranges()

    if "--self-test" in sys.argv:
        return self_test(ambiguous) or self_test_ranges()

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
