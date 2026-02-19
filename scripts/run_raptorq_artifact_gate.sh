#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: run_raptorq_artifact_gate.sh [options]

Generate and validate deterministic RaptorQ sidecar/decode-proof artifacts for
durability-critical evidence files, with optional corruption simulation.

Options:
  --output-root <path>   Output root for run artifacts (default: artifacts/durability/raptorq_runs)
  --run-id <id>          Run identifier (default: local-<utc-timestamp>)
  --no-phase2c           Skip auto-discovery of artifacts/phase2c evidence bundles
  --no-corruption        Skip corruption simulation checks
  -h, --help             Show this help

Runner knobs (env):
  FR_RAPTORQ_RUNNER      local (default) or rch
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

RUNNER="${FR_RAPTORQ_RUNNER:-local}"
cmd=(cargo run -p fr-conformance --bin raptorq_artifact_gate -- "$@")

if [[ "$RUNNER" == "rch" ]]; then
  cmd=(~/.local/bin/rch exec -- "${cmd[@]}")
fi

printf 'cmd='
printf '%q ' "${cmd[@]}"
echo
"${cmd[@]}"
