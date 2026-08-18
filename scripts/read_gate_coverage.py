#!/usr/bin/env python3
"""Reconcile the read-gate SOURCE SCAN against the harness's SHAPE COVERAGE.

WHY THIS EXISTS
---------------
The read-gate vein was under-counted four times (`0874b6512` records all four), and every one
had the same shape: an instrument reported on a SAMPLE and the total was quoted as if it were a
POPULATION.

    d7c67e802   a peer's 24-shape sweep, read as the whole set
    b6968821d   a survey window that ran past the end of a 9-line function
    521985c7d   a source scan that knew two of the three derivation forms
    f30ba2ad7   a census over whichever shapes happened to exist

The two instruments are each incomplete alone and complete together:

  * the SOURCE SCAN finds every route that derives the gate, including ones with no shape --
    but it cannot tell a converted twin-form wrapper from an unconverted route, and it silently
    misses any derivation form it does not know about;
  * the CENSUS (`call_count_delta.py --callers plain_borrowed_default_key_read_allows`) reports
    what a route actually EXECUTES and cannot be fooled by form -- but it is blind to any route
    with no shape.

So this script does the cross-reference and, crucially, prints the set the census CANNOT SEE.
That set is the one that produced three of the four undercounts.

WHAT IT DOES NOT DO
-------------------
It does not run callgrind and it does not report calls/op -- that needs a built ELF and minutes
per shape. Run it first to learn WHICH shapes to census and which routes still need a shape;
then run the census itself. This half is static, takes under a second, and needs no build.
"""
import argparse
import re
import sys
from pathlib import Path

GATE = "self.plain_borrowed_default_key_read_allows(now_ms)"

# The three derivation forms seen so far. A fourth would be invisible here, which is exactly the
# failure mode that produced `521985c7d` -- so `--dump-unclassified` exists to surface any gate
# occurrence this classifier could not place.
FORM_A = "predicate tail"        # `self.plain_..._allows(now_ms)` as the last expression
FORM_B = "inline guard"          # `if !self.plain_..._allows(now_ms) {`
FORM_C = "bounds disjunct"       # `|| !self.plain_..._allows(now_ms)` as the LAST disjunct
FORM_TWIN = "twin wrapper"       # `let default_read_allowed = self.plain_..._allows(now_ms);`
                                 # then a call to the `_with_default_read_gate` twin. This route
                                 # IS converted -- the floor arm uses the twin and this wrapper is
                                 # only the fallback for a caller holding nothing cached. Counting
                                 # it as "deriving" is what made a raw scan report 61 when the
                                 # census said 23.


def scan_runtime(path: Path):
    """Every function that still derives the gate itself, with the form it uses."""
    lines = path.read_text(errors="replace").split("\n")
    fnre = re.compile(r"^    (pub )?fn (\w+)")
    cur, found, unclassified, twins = None, {}, [], set()
    for i, line in enumerate(lines):
        m = fnre.match(line)
        if m:
            cur = m.group(2)
        if GATE not in line:
            continue
        prev = lines[i - 1] if i else ""
        # already converted: the Option fallback, on this line or the one above
        if "unwrap_or_else" in line or "unwrap_or_else" in prev or "default_read_allowed" in prev:
            continue
        s = line.strip()
        if s.startswith("let default_read_allowed ="):
            twins.add(cur)          # converted via the twin form; not a target
            continue
        if s.startswith("|| !"):
            form = FORM_C
        elif s.startswith("if !"):
            form = FORM_B
        elif s.startswith("self."):
            form = FORM_A
        else:
            form = None
            unclassified.append((cur, i + 1, s[:80]))
        if form:
            found[cur] = form
    return found, unclassified, twins


def shape_commands(path: Path):
    """command -> [shape names] for every shape in the harness."""
    body = re.search(r"^SHAPES = \{(.*?)^\}", path.read_text(errors="replace"), re.S | re.M)
    if not body:
        return {}
    out = {}
    for name, cmd in re.findall(r'^\s{4}"([a-z0-9_]+)":\s*\(\s*\[[^\]]*\]\s*,\s*\[\s*"([A-Za-z_]+)"',
                                body.group(1), re.M):
        out.setdefault(cmd.upper(), []).append(name)
    return out


def command_of(fn: str) -> str:
    """Best-effort command name for a route, for coverage matching only."""
    s = re.sub(r"^(can_)?execute_plain_", "", fn)
    s = re.sub(r"_borrowed(_into)?$", "", s)
    s = re.sub(r"_(with_default_read_gate)$", "", s)
    return s.upper()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--runtime", default="crates/fr-runtime/src/lib.rs")
    ap.add_argument("--shapes", default="scripts/shape_instr_per_op.py")
    ap.add_argument("--dump-unclassified", action="store_true",
                    help="print gate occurrences the form classifier could not place -- a "
                         "non-empty list means a FOURTH derivation form exists and every "
                         "source-derived total is suspect")
    args = ap.parse_args()

    derivers, unclassified, twins = scan_runtime(Path(args.runtime))
    # the accessor itself is not a route
    derivers.pop('plain_borrowed_default_key_read_gate', None)
    cmds = shape_commands(Path(args.shapes))

    covered, invisible = [], []
    for fn, form in sorted(derivers.items()):
        c = command_of(fn)
        # EXACT match only. Prefix matching mapped `sdiff` -> SDIFFSTORE (a WRITE command) and
        # `getbit` -> GET, marking blind routes as covered -- a coverage checker that flatters
        # itself is worse than none.
        hit = [k for k in cmds if k == c]
        (covered if hit else invisible).append((fn, form, hit[0] if hit else None))

    print(f"{len(derivers)} functions still derive the read gate directly "
          f"({len(twins)} more use the converted TWIN form and are not targets)\n")
    by_form = {}
    for _, form, _ in covered + invisible:
        by_form[form] = by_form.get(form, 0) + 1
    for form, n in sorted(by_form.items()):
        print(f"    {n:3}  {form}")

    print(f"\nCENSUS CAN SEE THESE ({len(covered)}) -- a shape exists, so calls/op adjudicates:")
    for fn, form, c in covered:
        print(f"    {fn:56} {form:18} via {c}")

    print(f"\n*** CENSUS IS BLIND TO THESE ({len(invisible)}) -- no shape, so a census total "
          f"that omits them is an UNDERCOUNT:")
    for fn, form, _ in invisible:
        print(f"    {fn:56} {form}")
    if invisible:
        print("\n    Add a shape for each before quoting any total for this vein, and verify it "
              "returns a real\n    reply with scripts/verify_census_shapes.py -- a mis-specified "
              "shape measures the error path\n    just as reproducibly as a correct one measures "
              "the command.")

    if unclassified:
        print(f"\n!!! {len(unclassified)} gate occurrences could NOT be classified into the three "
              f"known forms.\n    A fourth form would make every source-derived total suspect. "
              f"Re-run with --dump-unclassified.")
        if args.dump_unclassified:
            for fn, ln, txt in unclassified:
                print(f"    {args.runtime}:{ln}  in {fn}\n        {txt}")

    # Non-zero when the instruments disagree about coverage, so this can gate a claim.
    return 1 if (invisible or unclassified) else 0


if __name__ == "__main__":
    sys.exit(main())
