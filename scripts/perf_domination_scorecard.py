#!/usr/bin/env python3
"""Perf-domination scorecard: turn the .bench-history baselines into a human verdict.

Reporting capstone for the perf apparatus. Reads the two machine-checkable baselines:
  .bench-history/comprehensive_bench.latest.json  (throughput: fr/redis ops ratio per
                                                    workload@pipeline-depth, from
                                                    perf_baseline_capture.py)
  .bench-history/memory_baseline.latest.json       (RAM: fr/redis RSS ratio per type,
                                                    from memory_baseline_capture.py)
and emits a markdown scorecard answering "is fr beating redis 7.2.4, measured honestly?":
per-cell win/loss, throughput geomean, RAM geomean, headline win-rate, and the cells where
fr still loses (the remaining domination gaps). Noisy throughput cells (cv_pct > 5) are
excluded from the verdict (not keep-eligible), matching the keep-gate.

Pure JSON->markdown; runs anywhere (no servers). Prints "run the baseline scripts first"
when a baseline is missing, so it is safe to land before the first batch capture.

Usage: perf_domination_scorecard.py [--out <path.md>]
"""
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BENCH = os.path.join(ROOT, ".bench-history")
THRU = os.path.join(BENCH, "comprehensive_bench.latest.json")
MEM = os.path.join(BENCH, "memory_baseline.latest.json")


def _load(path):
    try:
        with open(path) as fh:
            return json.load(fh)
    except Exception:
        return None


def _geomean(values):
    vals = [v for v in values if v and v > 0]
    if not vals:
        return None
    return math.exp(sum(math.log(v) for v in vals) / len(vals))


def main():
    out_path = None
    if "--out" in sys.argv:
        out_path = sys.argv[sys.argv.index("--out") + 1]

    thru = _load(THRU)
    mem = _load(MEM)
    lines = ["# FrankenRedis Perf-Domination Scorecard (vs redis 7.2.4)", ""]

    if not thru and not mem:
        lines += [
            "_No baselines captured yet._ Run, in batch/rch (release fr binary):",
            "",
            "```",
            "scripts/perf_baseline_capture.py <redis-server> <fr-release-bin>",
            "scripts/memory_baseline_capture.py <redis-server> <fr-release-bin>",
            "```",
            "",
            "then re-run this scorecard.",
        ]
        report = "\n".join(lines)
        print(report)
        if out_path:
            open(out_path, "w").write(report + "\n")
        return

    # ---- Throughput pillar ----
    if thru:
        cells = thru.get("cells", {})
        rated = {k: c for k, c in cells.items()
                 if isinstance(c, dict) and "fr_over_redis" in c and not c.get("noisy")}
        noisy = [k for k, c in cells.items() if isinstance(c, dict) and c.get("noisy")]
        wins = sum(1 for c in rated.values() if c["fr_over_redis"] >= 1.0)
        gm = _geomean([c["fr_over_redis"] for c in rated.values()])
        lines += [
            "## Throughput (fr/redis ops/sec; >=1.0 = fr wins)", "",
            f"- Cells rated: **{len(rated)}** (excluding {len(noisy)} noisy cv>5% cells)",
            f"- fr wins (>=1.0x): **{wins}/{len(rated)}**"
            + (f" ({100*wins//max(len(rated),1)}%)" if rated else ""),
            f"- Throughput geomean: **{gm:.3f}x**" if gm else "- Throughput geomean: n/a",
            "",
            "| workload@depth | fr/redis | fr cv% | verdict |",
            "|---|---|---|---|",
        ]
        for k in sorted(rated):
            c = rated[k]
            v = "WIN" if c["fr_over_redis"] >= 1.0 else "loss"
            lines.append(f"| {k} | {c['fr_over_redis']:.3f} | {c.get('fr_cv_pct','?')} | {v} |")
        losers = sorted((k for k, c in rated.items() if c["fr_over_redis"] < 1.0),
                        key=lambda k: rated[k]["fr_over_redis"])
        if losers:
            lines += ["", "**Throughput gaps (fr slower):** "
                      + ", ".join(f"{k}={rated[k]['fr_over_redis']:.2f}x" for k in losers[:10])]
        if noisy:
            lines += ["", f"_Noisy (excluded): {', '.join(sorted(noisy))}_"]
        lines.append("")
    else:
        lines += ["## Throughput", "", "_comprehensive_bench.latest.json missing — run perf_baseline_capture.py._", ""]

    # ---- RAM pillar ----
    if mem:
        cells = mem.get("cells", {})
        rated = {k: c for k, c in cells.items() if isinstance(c, dict) and "rss_ratio" in c}
        gm = _geomean([c["rss_ratio"] for c in rated.values()])
        wins = sum(1 for c in rated.values() if c["rss_ratio"] <= 1.0)
        lines += [
            "## Memory (fr/redis RSS; <=1.0 = fr wins)", "",
            f"- Types rated: **{len(rated)}**",
            f"- fr wins (<=1.0x RSS): **{wins}/{len(rated)}**",
            f"- RSS geomean: **{gm:.3f}x**" if gm else "- RSS geomean: n/a",
            "",
            "| data-type | fr/redis RSS | fr/redis used_memory | verdict |",
            "|---|---|---|---|",
        ]
        for k in sorted(rated):
            c = rated[k]
            v = "WIN" if c["rss_ratio"] <= 1.0 else "loss"
            lines.append(f"| {k} | {c['rss_ratio']:.3f} | {c.get('used_ratio','?')} | {v} |")
        losers = sorted((k for k, c in rated.items() if c["rss_ratio"] > 1.0),
                        key=lambda k: -rated[k]["rss_ratio"])
        if losers:
            lines += ["", "**RAM gaps (fr heavier):** "
                      + ", ".join(f"{k}={rated[k]['rss_ratio']:.2f}x" for k in losers[:10])]
        lines.append("")
    else:
        lines += ["## Memory", "", "_memory_baseline.latest.json missing — run memory_baseline_capture.py._", ""]

    report = "\n".join(lines)
    print(report)
    if out_path:
        open(out_path, "w").write(report + "\n")


if __name__ == "__main__":
    main()
