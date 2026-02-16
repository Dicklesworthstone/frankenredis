#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/check_coverage_flake_budget.sh <coverage_summary.json>

Description:
  Enforces bd-2wb.23 reliability budgets against the machine-readable
  coverage summary emitted by scripts/run_live_oracle_diff.sh.

Budget knobs (env):
  FR_COVERAGE_FLOOR        Minimum suite pass_rate ratio (default: 0.95)
  FR_PACKET_COVERAGE_FLOORS_JSON
                           Optional JSON map for per-packet floors
                           (example: {"FR-P2C-002":0.99,"FR-P2C-003":0.97})
  FR_FLAKE_CEILING         Max allowed flake_suspect_suites count (default: 0)
  FR_HARD_FAIL_CEILING     Max allowed hard_fail_suites count (default: 0)
  FR_CASE_FAILURE_CEILING  Max allowed total_case_failures (default: 0)
  FR_BUDGET_RESULT_PATH    Optional output JSON path for gate result
  FR_QUARANTINE_PATH       Optional path for flake quarantine candidate list
USAGE
}

if (($# != 1)); then
  usage >&2
  exit 2
fi

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

SUMMARY_PATH="$1"
if [[ ! -f "$SUMMARY_PATH" ]]; then
  echo "missing summary file: $SUMMARY_PATH" >&2
  exit 2
fi

RESULT_PATH="${FR_BUDGET_RESULT_PATH:-$(dirname "$SUMMARY_PATH")/reliability_budget_result.json}"
COVERAGE_FLOOR="${FR_COVERAGE_FLOOR:-0.95}"
FLAKE_CEILING="${FR_FLAKE_CEILING:-0}"
HARD_FAIL_CEILING="${FR_HARD_FAIL_CEILING:-0}"
CASE_FAILURE_CEILING="${FR_CASE_FAILURE_CEILING:-0}"
PACKET_COVERAGE_FLOORS_JSON="${FR_PACKET_COVERAGE_FLOORS_JSON-}"
if [[ -z "$PACKET_COVERAGE_FLOORS_JSON" ]]; then
  PACKET_COVERAGE_FLOORS_JSON='{}'
fi
QUARANTINE_PATH="${FR_QUARANTINE_PATH:-$(dirname "$SUMMARY_PATH")/flake_quarantine_candidates.txt}"

python3 - "$SUMMARY_PATH" "$RESULT_PATH" "$COVERAGE_FLOOR" "$FLAKE_CEILING" "$HARD_FAIL_CEILING" "$CASE_FAILURE_CEILING" "$PACKET_COVERAGE_FLOORS_JSON" "$QUARANTINE_PATH" <<'PY'
import json
import sys
from pathlib import Path

summary_path = Path(sys.argv[1])
result_path = Path(sys.argv[2])
coverage_floor = float(sys.argv[3])
flake_ceiling = int(sys.argv[4])
hard_fail_ceiling = int(sys.argv[5])
case_failure_ceiling = int(sys.argv[6])
packet_coverage_floors_raw = sys.argv[7]
quarantine_path = Path(sys.argv[8])

summary = json.loads(summary_path.read_text(encoding="utf-8"))
try:
    parsed_packet_floors = json.loads(packet_coverage_floors_raw)
except json.JSONDecodeError as exc:
    raise SystemExit(f"invalid FR_PACKET_COVERAGE_FLOORS_JSON: {exc}") from exc
if not isinstance(parsed_packet_floors, dict):
    raise SystemExit("FR_PACKET_COVERAGE_FLOORS_JSON must decode to a JSON object")
packet_coverage_floors = {
    str(key): float(value) for key, value in parsed_packet_floors.items()
}

pass_rate = float(summary.get("pass_rate", 0.0) or 0.0)
failed_suites = int(summary.get("failed_suites", 0) or 0)
total_case_failures = int(summary.get("total_case_failures", 0) or 0)
flake_suspects = list(summary.get("flake_suspect_suites") or [])
hard_fail_suites = list(summary.get("hard_fail_suites") or [])
packet_family_pass_rates = list(summary.get("packet_family_pass_rates") or [])

violations = []
if pass_rate + 1e-9 < coverage_floor:
    violations.append(
        f"coverage floor violated: pass_rate={pass_rate:.4f} < floor={coverage_floor:.4f}"
    )
if len(flake_suspects) > flake_ceiling:
    violations.append(
        "flake ceiling violated: "
        f"flake_suspect_suites={len(flake_suspects)} > ceiling={flake_ceiling}"
    )
if len(hard_fail_suites) > hard_fail_ceiling:
    violations.append(
        "hard-fail ceiling violated: "
        f"hard_fail_suites={len(hard_fail_suites)} > ceiling={hard_fail_ceiling}"
    )
if total_case_failures > case_failure_ceiling:
    violations.append(
        "case-failure ceiling violated: "
        f"total_case_failures={total_case_failures} > ceiling={case_failure_ceiling}"
    )

packet_thresholds = []
for packet in packet_family_pass_rates:
    packet_id = str(packet.get("packet_id"))
    packet_pass_rate = float(packet.get("pass_rate", 0.0) or 0.0)
    packet_floor = float(packet_coverage_floors.get(packet_id, coverage_floor))
    packet_thresholds.append(
        {
            "packet_id": packet_id,
            "pass_rate": round(packet_pass_rate, 4),
            "floor": round(packet_floor, 4),
        }
    )
    if packet_pass_rate + 1e-9 < packet_floor:
        violations.append(
            "packet coverage floor violated: "
            f"packet={packet_id} pass_rate={packet_pass_rate:.4f} < floor={packet_floor:.4f}"
        )

status = "pass" if not violations else "fail"
quarantine_candidates = sorted(set(flake_suspects))
if quarantine_candidates:
    quarantine_path.parent.mkdir(parents=True, exist_ok=True)
    quarantine_path.write_text("\n".join(quarantine_candidates) + "\n", encoding="utf-8")

result = {
    "schema_version": "live_oracle_budget_gate/v1",
    "bead_id": "bd-2wb.23",
    "status": status,
    "summary_path": str(summary_path),
    "thresholds": {
        "coverage_floor": coverage_floor,
        "packet_coverage_floors": packet_coverage_floors,
        "flake_ceiling": flake_ceiling,
        "hard_fail_ceiling": hard_fail_ceiling,
        "case_failure_ceiling": case_failure_ceiling,
    },
    "metrics": {
        "run_id": summary.get("run_id"),
        "total_suites": int(summary.get("total_suites", 0) or 0),
        "passed_suites": int(summary.get("passed_suites", 0) or 0),
        "failed_suites": failed_suites,
        "pass_rate": round(pass_rate, 4),
        "total_case_failures": total_case_failures,
        "flake_suspect_suites": flake_suspects,
        "hard_fail_suites": hard_fail_suites,
        "packet_family_thresholds": packet_thresholds,
        "primary_reason_codes": summary.get("primary_reason_codes") or [],
    },
    "violations": violations,
    "remediation": {
        "readme_path": summary.get("readme_path"),
        "replay_script": summary.get("replay_script"),
        "quarantine_candidates_path": str(quarantine_path) if quarantine_candidates else None,
        "quarantine_candidates": quarantine_candidates,
        "next_steps": [
            "Inspect suite_status.tsv and suite report JSON files under run_root.",
            "Run replay_failed.sh to reproduce failing suites deterministically.",
            "Use per-suite reason_code_counts to route ownership and open/advance packet beads.",
        ],
    },
}

result_path.parent.mkdir(parents=True, exist_ok=True)
result_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")

print(f"budget_result: {result_path}")
print(f"status: {status}")
print(
    "metrics: "
    f"pass_rate={pass_rate:.4f}, failed_suites={failed_suites}, "
    f"flake_suspects={len(flake_suspects)}, hard_fails={len(hard_fail_suites)}, "
    f"total_case_failures={total_case_failures}"
)
if quarantine_candidates:
    print(f"quarantine_candidates: {quarantine_path}")
if violations:
    print("violations:")
    for violation in violations:
        print(f"- {violation}")
    print("remediation:")
    print(f"- readme: {summary.get('readme_path')}")
    print(f"- replay: {summary.get('replay_script')}")
    sys.exit(1)

sys.exit(0)
PY
