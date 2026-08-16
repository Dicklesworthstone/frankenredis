#!/usr/bin/env python3
"""Per-op re-parse census: how many times does fr parse each packet?

(frankenredis-7xa4m) Result of running this across 12 routes: dispatch cost is
linear in the parse count, dispatch = 284.2 * parses + 69.3, mean |residual|
281.6 instr/op over a 339-4723 range. GET and EXISTS do ZERO parses. Use it to
check a dispatch candidate: the coefficient predicts ~284 instructions saved per
re-parse eliminated, which is falsifiable.

(frankenredis-ddriz) The cascade calls a shape-parser per CANDIDATE ARM rather
than parsing once and dispatching. PERSIST calls the generic key_arg2 parser 7.00
times per command against the identical two-token packet. This counts that for
every route, so the mechanism is enumerated rather than sampled.
"""
import os
import re
import subprocess
import sys

ROOT = "/data/projects/frankenredis"
OPS = 2000  # harness uses N and 2N; the 2N dump therefore covers 2*OPS ops


def run_shape(fr_bin, shape):
    out = subprocess.run(
        [sys.executable, os.path.join(ROOT, "scripts", "shape_instr_per_op.py"),
         fr_bin, shape, str(OPS), "--fr-only"],
        capture_output=True, text=True, timeout=1800, cwd=ROOT).stdout
    ipo = disp = None
    m = re.search(r"fr\s+([\d.]+) instr/op\s+dispatch\s+([\d.]+)", out)
    if m:
        ipo, disp = float(m.group(1)), float(m.group(2))
    d = re.search(r"callgrind dumps: (\S+)", out)
    return ipo, disp, (d.group(1) if d else None)


def parse_counts(dump_dir):
    """calls-per-op of each parse_borrowed_plain_* parser, from the 2N dump."""
    path = os.path.join(dump_dir, "cg.fr.2n.out")
    out = subprocess.run(["callgrind_annotate", "--tree=caller", "--threshold=99.5", path],
                         capture_output=True, text=True, timeout=900).stdout
    counts = {}
    for m in re.finditer(r"parse_borrowed_plain_([a-z0-9_]+)_packet \(([\d,]+)x\)", out):
        name, n = m.group(1), int(m.group(2).replace(",", ""))
        counts[name] = max(counts.get(name, 0), n)
    per_op = {k: v / (2 * OPS) for k, v in counts.items() if v >= 2 * OPS * 0.5}
    # (frankenredis-5b596) A per-op parser call count MUST be a whole number:
    # the parser either runs n times per command or it does not. A fractional
    # value means the N and 2N runs did not scale, i.e. the two-point subtraction
    # was contaminated and the whole row is invalid. Observed live: three repeats
    # of ZRANGEBYSCORE-with-LIMIT gave 81.0, 81.0 and 66.4, and the 66.4 run also
    # reported 9179 instr/op against the other two at 22073 and 22106. The
    # fraction is the cheaper signal -- it is visible without a second run.
    for name, calls in sorted(per_op.items()):
        if abs(calls - round(calls)) > 0.02:
            raise SystemExit(
                "INVALID: %s measured %.2f calls per op, which is not a whole "
                "number. The two-point subtraction did not scale between the N "
                "and 2N runs, so every figure in this row is contaminated. "
                "Re-run." % (name, calls))
    return per_op


def main():
    fr_bin = os.path.abspath(sys.argv[1])
    shapes = sys.argv[2:]
    print("%-16s %9s %9s  %6s  %s" % ("route", "instr/op", "dispatch", "parses", "parsers (calls/op)"))
    for shape in shapes:
        ipo, disp, dump = run_shape(fr_bin, shape)
        if dump is None:
            print("%-16s  (no dump -- shape not registered?)" % shape)
            continue
        counts = parse_counts(dump)
        total = sum(counts.values())
        detail = " ".join("%s=%.1f" % (k, v) for k, v in
                          sorted(counts.items(), key=lambda x: -x[1])[:4])
        print("%-16s %9.1f %9.1f  %6.1f  %s" % (shape, ipo or -1, disp or -1, total, detail))


if __name__ == "__main__":
    main()
