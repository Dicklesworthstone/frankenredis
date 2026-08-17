#!/usr/bin/env python3
"""Decide whether the host is in a state where a measurement means what it claims.

(frankenredis-ozrro) This exists because four consecutive fleet-level "clean window" reports
did not reproduce on my own `uptime`/`pgrep` check, and each time the difference changed what I
did. Hand-checking works but is easy to skip on the tick where it matters, so the check is
mechanical here.

THE GATE IS DIFFERENT FOR THE TWO KINDS OF MEASUREMENT, and conflating them is why a single
blunt threshold would be wrong:

  fr-only   fr's instruction count is essentially load-immune. `dump_small`'s fr arm reads
            2,686.3-2,703.7 across SIX sessions spanning loadavg 14 to 66 — 0.65 pct total
            spread. A dirty window costs wall-clock, not accuracy. Gate loosely.

  ratio     the REDIS denominator is what moves. The same control's redis arm reads
            5,045.8 / 5,046.6 / 5,050.8 / 5,345.9 — the last 5.9 pct high, measured with four
            peer builds running. An earlier row put the variation at 1.6-4.0 pct and showed it
            does NOT track load monotonically, so this cannot be corrected for after the fact.
            Gate strictly, and refuse outright when builds are running.

WHAT IT CHECKS
  * cargo/rustc processes anywhere on the host. These run under the SHARED `ubuntu` uid, so
    ownership cannot be attributed — any of them is disqualifying for a ratio.
  * STATIONARITY, not absolute load: |1min - 5min| / 5min. A decaying window is the common
    trap — the 1-minute looks reassuring while the 5- and 15-minute say the storm is still
    draining, and a run started there straddles two regimes.
  * the 15-minute average, as evidence the host has actually settled rather than dipped.

It deliberately does NOT hard-fail on absolute load alone. This campaign's most reproducible
numbers were taken at loadavg 14-24, and refusing those would have discarded good work — the
frankenpandas failure this ledger has a row about, in a different costume.

Usage:
    certification_window.py --for ratio     # strict; exit 2 if unfit
    certification_window.py --for fr-only   # lenient
    certification_window.py --self-test
Exit: 0 fit, 2 unfit (reasons on stdout).
"""

from __future__ import annotations

import glob
import os
import subprocess
import sys

# Stationarity: how far the 1-minute may sit from the 5-minute, as a fraction of the 5-minute.
# 0.15 admits the windows this campaign certified in (2-6 pct apart) and rejects the decaying
# ones it refused (40 pct+).
RATIO_MAX_DRIFT = 0.15
FR_ONLY_MAX_DRIFT = 0.60
# The 15-minute is the settled-ness witness: a low 1-minute with a huge 15-minute is a dip.
RATIO_MAX_15MIN = 30.0


def loadavg():
    with open("/proc/loadavg") as fh:
        parts = fh.read().split()
    return float(parts[0]), float(parts[1]), float(parts[2])


def build_processes():
    """cargo/rustc process command lines, or [] if none. Shared uid, so not attributable."""
    r = subprocess.run(
        ["pgrep", "-a", "-u", str(os.getuid()), "cargo|rustc"],
        capture_output=True, text=True, check=False,
    )
    return [l for l in r.stdout.splitlines() if l.strip()]


def cpu_mhz(limit=8):
    out = []
    for path in sorted(glob.glob("/sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_cur_freq"))[:limit]:
        try:
            with open(path) as fh:
                out.append(int(fh.read().strip()) // 1000)
        except OSError:
            pass
    if out:
        return out
    # Fall back to /proc/cpuinfo, which is what the campaign's rows record.
    try:
        with open("/proc/cpuinfo") as fh:
            for line in fh:
                if line.startswith("cpu MHz"):
                    out.append(int(float(line.split(":")[1])))
                    if len(out) >= limit:
                        break
    except OSError:
        pass
    return out


def verdict(kind, one, five, fifteen, builds):
    """(fit, reasons). `kind` is 'ratio' or 'fr-only'."""
    reasons = []
    strict = kind == "ratio"
    drift = abs(one - five) / five if five else 0.0
    limit = RATIO_MAX_DRIFT if strict else FR_ONLY_MAX_DRIFT

    if strict and builds:
        reasons.append(
            f"{len(builds)} cargo/rustc process(es) running — disqualifying for a ratio "
            f"(shared uid, so none can be attributed away)"
        )
    elif builds:
        reasons.append(f"NOTE: {len(builds)} cargo/rustc process(es) running; fine for fr-only")

    if drift > limit:
        reasons.append(
            f"non-stationary: 1min {one:.2f} vs 5min {five:.2f} = {100 * drift:.0f} pct apart "
            f"(limit {100 * limit:.0f} pct)"
        )
    if strict and fifteen > RATIO_MAX_15MIN:
        reasons.append(
            f"15min {fifteen:.2f} above {RATIO_MAX_15MIN:.0f} — host has not settled, a low "
            f"1min here is a dip rather than calm"
        )

    disqualifying = [r for r in reasons if not r.startswith("NOTE:")]
    return (not disqualifying), reasons


def main():
    kind = "ratio"
    if "--for" in sys.argv:
        kind = sys.argv[sys.argv.index("--for") + 1]
    if kind not in ("ratio", "fr-only"):
        print("usage: certification_window.py --for {ratio|fr-only}", file=sys.stderr)
        return 2

    one, five, fifteen = loadavg()
    builds = build_processes()
    fit, reasons = verdict(kind, one, five, fifteen, builds)
    mhz = cpu_mhz()

    print(f"loadavg {one:.2f} / {five:.2f} / {fifteen:.2f}")
    print(f"cpu MHz {' '.join(str(m) for m in mhz) if mhz else 'unavailable'}")
    print(f"builds  {len(builds)}")
    for r in reasons:
        print(f"  - {r}")
    print(f"VERDICT for {kind}: {'FIT' if fit else 'UNFIT'}")
    if not fit:
        print("  Record the per-arm loadavg and MHz above with any number taken anyway,")
        print("  and label it sizing rather than certified.")
    return 0 if fit else 2


def _self_test():
    # A ratio in a window with builds is refused no matter how calm the averages look.
    fit, why = verdict("ratio", 2.0, 2.0, 2.0, ["cargo test"])
    assert not fit and any("disqualifying" in w for w in why), why
    # The same window is FINE for fr-only, because fr's Ir is load-immune (0.65 pct over six
    # sessions spanning loadavg 14-66).
    fit, why = verdict("fr-only", 2.0, 2.0, 2.0, ["cargo test"])
    assert fit and any(w.startswith("NOTE:") for w in why), why

    # The decaying-window trap: a reassuring 1-minute over a much larger 5-minute.
    fit, _ = verdict("ratio", 18.0, 25.2, 44.8, [])
    assert not fit, "18/25/45 must be refused for a ratio"
    # Calm and settled passes.
    fit, why = verdict("ratio", 12.0, 12.5, 13.0, [])
    assert fit, why

    # A low 1-minute under a still-huge 15-minute is a dip, not calm.
    fit, _ = verdict("ratio", 5.0, 5.2, 90.0, [])
    assert not fit, "a dip under a 90 15-min average must be refused"

    # Must NOT refuse on absolute load alone: this campaign's most reproducible rows were taken
    # at loadavg 14-24, and refusing them would discard good work.
    fit, why = verdict("ratio", 22.0, 22.5, 24.0, [])
    assert fit, f"steady load 22 must be usable: {why}"

    # fr-only tolerates real drift, since accuracy does not depend on it.
    fit, _ = verdict("fr-only", 40.0, 60.0, 90.0, [])
    assert fit, "fr-only must tolerate a drifting window"

    assert loadavg()[0] >= 0.0
    print("self-test ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(_self_test() if "--self-test" in sys.argv else main())
