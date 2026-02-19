#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/run_live_oracle_diff.sh [--host <host>] [--port <port>] [--output-root <dir>] [--run-id <id>]
  ./scripts/run_live_oracle_diff.sh [host] [port]

Description:
  Thin wrapper around the Rust orchestrator:
  cargo run -p fr-conformance --bin live_oracle_orchestrator -- [args]

Runner knobs (env):
  FR_E2E_RUNNER          local (default) or rch (applied inside orchestrator)
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

cmd=(cargo run -p fr-conformance --bin live_oracle_orchestrator -- "$@")
printf 'cmd='
printf '%q ' "${cmd[@]}"
echo
"${cmd[@]}"
