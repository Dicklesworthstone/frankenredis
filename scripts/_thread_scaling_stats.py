#!/usr/bin/env python3
"""Curve statistics for thread_scaling_headtohead.sh.

One row per (workers, round): workers, fr_opss, rd_opss, fr2_opss, fr_threads.

The verdict per worker count is gated on a bootstrap 95% median CI of the
per-round fr/redis ratios against the same-invocation A/A null's own bootstrap
CI, with the campaign's 2x margin rule: the effect's distance from 1.0 must be
at least twice the null's worst deviation from 1.0. CV is never computed and
never gates.

Reuses the deterministic percentile-bootstrap from perf_baseline_capture so the
same resampler backs every gated number in this repository.
"""
import statistics as st
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from perf_baseline_capture import _bootstrap_median_ci  # noqa: E402


def fmt(value):
    return f"{value:.4f}"


def main(path):
    rows = []
    for line in open(path):
        parts = line.rstrip("\n").split("\t")
        if len(parts) != 5:
            continue
        try:
            workers = int(parts[0])
            fr, rd, fr2 = (float(parts[1]), float(parts[2]), float(parts[3]))
            threads = int(parts[4])
        except ValueError:
            continue
        if min(fr, rd, fr2) <= 0:
            continue
        rows.append((workers, fr, rd, fr2, threads))
    if not rows:
        print("no usable rows")
        return 1

    worker_counts = sorted({r[0] for r in rows})
    print()
    print(f"{'workers':>8} {'obs_thr':>8} {'fr ops/s':>12} {'redis ops/s':>12} "
          f"{'fr/redis':>9} {'95% CI':>19} {'A/A null':>9} {'null CI':>19} {'verdict':>12}")
    curve = []
    for workers in worker_counts:
        group = [r for r in rows if r[0] == workers]
        ratios = [r[1] / r[2] for r in group]
        nulls = [r[3] / r[1] for r in group]
        threads = st.median(r[4] for r in group)
        eff = st.median(ratios)
        eff_lo, eff_hi = _bootstrap_median_ci(ratios)
        nul = st.median(nulls)
        nul_lo, nul_hi = _bootstrap_median_ci(nulls)

        # Gate, in three clauses (2026-07-30 fleet-corrected form):
        #   1. the effect CI must be disjoint from the null CI;
        #   2. the effect deviation must clear 2x the null's worst deviation;
        #   3. the null MEDIAN must sit within 2% of 1.0.
        #
        # Clause 3 was missing here and it mattered. On 2026-07-30 a W=8 row scored
        # DECIDABLE on a null whose MEDIAN was 0.9087 -- two identical binaries
        # differing 9.1% -- because clauses 1 and 2 can both pass while the null is
        # badly biased, so long as it is biased CONSISTENTLY. Clause 3 bounds
        # arm-order bias directly.
        #
        # Deliberately NOT added: a veto when the null CI fails to straddle 1.0.
        # That couples the verdict to the null's PRECISION with the sign inverted --
        # a tighter, better null yields a narrower CI, which is MORE likely to fall
        # entirely on one side of 1.0 and veto its own row. Audited here on
        # 2026-07-30: our tightest null (0.073% spread, effect 1997x its half-width)
        # straddles 1.0 only by a 3-above/2-below sign balance, and none of five
        # leave-one-out subsets flipped it, so the verdicts are stable and no
        # straddle clause is warranted. Null CIs are telemetry, never a veto.
        null_dev = max(abs(nul_hi - 1.0), abs(nul_lo - 1.0), 1e-9)
        eff_dev = abs(eff - 1.0)
        separated = eff_lo > nul_hi or eff_hi < nul_lo
        null_median_ok = abs(nul - 1.0) <= 0.02
        if not null_median_ok:
            verdict = f"NULL-BIAS {nul:.3f}"
        elif not separated:
            verdict = "IN-NULL"
        elif eff_dev >= 2.0 * null_dev:
            verdict = "DECIDABLE"
        else:
            verdict = "THIN"
        print(f"{workers:>8} {threads:>8.0f} {st.median(r[1] for r in group):>12,.0f} "
              f"{st.median(r[2] for r in group):>12,.0f} {eff:>9.4f} "
              f"{'[' + fmt(eff_lo) + ',' + fmt(eff_hi) + ']':>19} {nul:>9.4f} "
              f"{'[' + fmt(nul_lo) + ',' + fmt(nul_hi) + ']':>19} {verdict:>12}")
        curve.append((workers, threads, eff, verdict))

    print()
    print("== curve shape ==")
    decidable = [c for c in curve if c[3] == "DECIDABLE"]
    if len(curve) >= 2:
        first, last = curve[0], curve[-1]
        print(f"  workers {first[0]} -> {last[0]}: ratio {first[2]:.4f} -> {last[2]:.4f}")
        best = max(curve, key=lambda c: c[2])
        print(f"  peak ratio {best[2]:.4f} at {best[0]} requested workers "
              f"({best[1]:.0f} observed threads)")
        if best[0] != last[0]:
            print(f"  SHAPE: ratio PEAKS at {best[0]} workers and does not improve "
                  f"beyond it -- name what saturates first.")
        elif last[2] > first[2]:
            print("  SHAPE: ratio still RISING at the top of the sweep -- "
                  "headroom is not exhausted.")
        else:
            print("  SHAPE: flat or falling across the sweep.")
    if not decidable:
        print("  NOTE: no worker count produced a DECIDABLE ratio; "
              "nothing here is bankable.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
