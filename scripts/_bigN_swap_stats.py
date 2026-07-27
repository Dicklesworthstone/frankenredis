#!/usr/bin/env python3
"""Swap-design statistics for bigN_swap.sh.

Effect cancels the per-core factor exactly:
    E = sqrt( (FR_X * FR_Y) / (RD_X * RD_Y) )

The A/A null uses the SAME estimator shape on same-engine measurements taken in
different rounds, so it reports residual noise on the same scale as the effect
rather than a differently-constructed number the effect can be compared against
unfairly.
"""
import math
import statistics as st
import sys


def band(values):
    if not values:
        return "n/a"
    return f"{st.median(values):.4f} [{min(values):.4f}, {max(values):.4f}]"


def main(path):
    rows = [l.rstrip("\n").split("\t") for l in open(path)]
    rows = [r for r in rows if len(r) == 5 and min(float(x) for x in r[1:]) > 0]
    if len(rows) < 2:
        print("insufficient rows")
        return 1
    eff = [math.sqrt((float(r[1]) * float(r[2])) / (float(r[3]) * float(r[4])))
           for r in rows]
    # A/A null: same estimator, fr-only, across consecutive round pairs.
    nul = []
    for i in range(len(rows) - 1):
        a1, a2 = float(rows[i][1]), float(rows[i][2])
        b1, b2 = float(rows[i + 1][1]), float(rows[i + 1][2])
        nul.append(math.sqrt((b1 * b2) / (a1 * a2)))
    print(f"  effect  E = sqrt(FR_X*FR_Y / RD_X*RD_Y) = {band(eff)}")
    print(f"  A/A null (fr round-to-round, same estimator) = {band(nul)}")
    m = st.median(eff)
    if nul:
        lo, hi = min(nul), max(nul)
        decidable = m > hi or m < lo
        print(f"  -> {'DECIDABLE' if decidable else 'INSIDE THE NULL - NOT DECIDABLE'}")
        if decidable:
            margin = (m - 1) / max(abs(hi - 1), abs(lo - 1), 1e-9)
            print(f"     effect is {margin:.1f}x the null's worst deviation "
                  f"(campaign bar is 2x)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
