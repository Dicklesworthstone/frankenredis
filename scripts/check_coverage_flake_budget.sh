#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/check_coverage_flake_budget.sh <coverage_summary.json>
  FR_BUDGET_RUNNER=rch ./scripts/check_coverage_flake_budget.sh <coverage_summary.json>

Description:
  Thin wrapper around the Rust orchestrator:
  cargo run -p fr-conformance --bin live_oracle_budget_orchestrator -- <coverage_summary.json>

Runner knobs (env):
  FR_BUDGET_RUNNER         local (default) or rch
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || $# -ne 1 ]]; then
  usage
  [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]] && exit 0
  exit 2
fi

cmd=(cargo run -p fr-conformance --bin live_oracle_budget_orchestrator -- "$1")

printf 'cmd='
printf '%q ' "${cmd[@]}"
echo
"${cmd[@]}"
