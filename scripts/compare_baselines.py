#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compare two FrankenRedis benchmark baseline JSON files."
    )
    parser.add_argument("baseline_a", type=Path, help="Reference baseline JSON path")
    parser.add_argument("baseline_b", type=Path, help="Candidate baseline JSON path")
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable JSON instead of the text report",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as handle:
        return json.load(handle)


def normalize(report: dict[str, Any], source: Path) -> dict[str, Any]:
    schema = report.get("schema_version")
    if schema == "frankenredis_baseline/v1":
        return {
            "source": str(source),
            "server": report["server"],
            "server_version": report["server_version"],
            "workload": report["workload"],
            "clients": report["clients"],
            "pipeline": report["pipeline"],
            "total_requests": report["total_requests"],
            "ops_sec": float(report["ops_sec"]),
            "p50_us": float(report["p50_us"]),
            "p95_us": float(report["p95_us"]),
            "p99_us": float(report["p99_us"]),
            "p999_us": float(report["p999_us"]),
            "total_time_sec": float(report["total_time_sec"]),
        }

    if schema == "fr_bench_report/v1":
        latency = report["latency_us"]
        return {
            "source": str(source),
            "server": "unknown",
            "server_version": "unknown",
            "workload": report["workload"],
            "clients": report["clients"],
            "pipeline": report["pipeline"],
            "total_requests": report["requests"],
            "ops_sec": float(report["ops_per_sec"]),
            "p50_us": float(latency["p50"]),
            "p95_us": float(latency["p95"]),
            "p99_us": float(latency["p99"]),
            "p999_us": float(latency["p999"]),
            "total_time_sec": float(report["total_time_ms"]) / 1000.0,
        }

    raise ValueError(f"unsupported benchmark schema in {source}: {schema!r}")


def percent_delta(reference: float, candidate: float) -> float:
    if math.isclose(reference, 0.0):
        return math.inf if not math.isclose(candidate, 0.0) else 0.0
    return ((candidate - reference) / reference) * 100.0


def compare(reference: dict[str, Any], candidate: dict[str, Any]) -> dict[str, Any]:
    metrics = {}
    for key in ("ops_sec", "p50_us", "p95_us", "p99_us", "p999_us", "total_time_sec"):
        ref_value = float(reference[key])
        cand_value = float(candidate[key])
        metrics[key] = {
            "reference": ref_value,
            "candidate": cand_value,
            "delta": cand_value - ref_value,
            "delta_pct": percent_delta(ref_value, cand_value),
        }

    return {
        "reference": reference,
        "candidate": candidate,
        "metrics": metrics,
    }


def render_text(result: dict[str, Any]) -> str:
    reference = result["reference"]
    candidate = result["candidate"]
    metrics = result["metrics"]

    lines = [
        "baseline comparison",
        f"reference: {reference['source']}",
        f"candidate: {candidate['source']}",
        (
            f"workload: {reference['workload']}  "
            f"clients={reference['clients']}  "
            f"pipeline={reference['pipeline']}  "
            f"requests={reference['total_requests']}"
        ),
        "",
    ]

    for key, label in (
        ("ops_sec", "throughput ops/sec"),
        ("p50_us", "p50 latency us"),
        ("p95_us", "p95 latency us"),
        ("p99_us", "p99 latency us"),
        ("p999_us", "p999 latency us"),
        ("total_time_sec", "total time sec"),
    ):
        metric = metrics[key]
        lines.append(
            (
                f"{label}: ref={metric['reference']:.3f}  "
                f"cand={metric['candidate']:.3f}  "
                f"delta={metric['delta']:+.3f}  "
                f"delta_pct={metric['delta_pct']:+.2f}%"
            )
        )

    return "\n".join(lines)


def main() -> int:
    args = parse_args()
    reference = normalize(load_json(args.baseline_a), args.baseline_a)
    candidate = normalize(load_json(args.baseline_b), args.baseline_b)
    if (
        reference["workload"] != candidate["workload"]
        or reference["pipeline"] != candidate["pipeline"]
    ):
        raise SystemExit(
            "workload mismatch: compare only like-for-like benchmark runs"
        )
    result = compare(reference, candidate)

    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
    else:
        print(render_text(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
