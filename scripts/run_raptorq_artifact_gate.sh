#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: run_raptorq_artifact_gate.sh [options]

Thin wrapper around the Rust orchestrator:
  cargo run -p fr-conformance --bin raptorq_artifact_orchestrator -- [options]

Options:
  --runner <local|rch>   Execution backend override (default from FR_RAPTORQ_RUNNER)
  [other options]        Forwarded to raptorq_artifact_gate
  -h, --help             Show this help

Runner knobs (env):
  FR_RAPTORQ_RUNNER      local (default) or rch
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

cmd=(cargo run -p fr-conformance --bin raptorq_artifact_orchestrator -- "$@")

printf 'cmd='
printf '%q ' "${cmd[@]}"
echo
"${cmd[@]}"
