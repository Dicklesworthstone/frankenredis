#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/run_adversarial_triage.sh [--manifest <path>] [--output-root <dir>] [--run-id <id>] [--runner <rch|local>]

Description:
  Thin wrapper around the Rust orchestrator:
  cargo run -p fr-conformance --bin adversarial_triage_orchestrator -- [args]

Runner knobs (env):
  FR_ADV_MANIFEST       default manifest path
  FR_ADV_OUTPUT_ROOT    default output root
  FR_ADV_RUN_ID         default run id
  FR_ADV_RUNNER         default runner (rch|local, default: rch)
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

cmd=(cargo run -p fr-conformance --bin adversarial_triage_orchestrator -- "$@")
printf 'cmd='
printf '%q ' "${cmd[@]}"
echo
"${cmd[@]}"
