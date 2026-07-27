#!/usr/bin/env python3
"""Print one aggregated row for bigN_collection_read.sh.

Statistic is the median of per-round values, and the reported ratio is the median
of per-round RATIOS (not a ratio of medians), so a slow round cannot bias one arm.
"""
import statistics as st
import sys


def main(path, n):
    rows = [line.rstrip("\n").split("\t") for line in open(path)]
    rows = [r for r in rows if len(r) == 5 and r[0] == n]
    if not rows:
        print(f"{n:<9} (no rows)")
        return 0
    fr = st.median(float(r[2]) for r in rows)
    rd = st.median(float(r[3]) for r in rows)
    aa = st.median(float(r[4]) for r in rows)
    ratios = [float(r[2]) / float(r[3]) for r in rows if float(r[3])]
    nulls = [float(r[4]) / float(r[2]) for r in rows if float(r[2])]
    ratio = st.median(ratios) if ratios else 0.0
    null = st.median(nulls) if nulls else 0.0
    print(f"{n:<9} {fr:>12,.0f} {rd:>12,.0f} {aa:>12,.0f} {ratio:>9.3f} {null:>9.3f}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
