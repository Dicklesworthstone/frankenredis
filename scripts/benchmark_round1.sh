#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/benchmark_round1.sh

Description:
  Thin wrapper around:
  cargo run -p fr-conformance --bin conformance_benchmark_runner -- --round round1
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

cmd=(cargo run -p fr-conformance --bin conformance_benchmark_runner -- --round round1 "$@")
printf 'cmd='
printf '%q ' "${cmd[@]}"
echo
"${cmd[@]}"
