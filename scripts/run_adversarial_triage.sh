#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/run_adversarial_triage.sh [--manifest <path>] [--output-root <dir>] [--run-id <id>] [--runner <rch|local>]

Description:
  Executes the versioned adversarial corpus through fr-conformance and emits a
  deterministic triage bundle with failure classification and bead routing.
USAGE
}

MANIFEST="${FR_ADV_MANIFEST:-crates/fr-conformance/fixtures/adversarial_corpus_v1.json}"
OUTPUT_ROOT="${FR_ADV_OUTPUT_ROOT:-artifacts/adversarial_triage}"
RUN_ID="${FR_ADV_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
RUNNER="${FR_ADV_RUNNER:-rch}"

while (($# > 0)); do
  case "$1" in
    --manifest)
      MANIFEST="${2:-}"
      shift 2
      ;;
    --output-root)
      OUTPUT_ROOT="${2:-}"
      shift 2
      ;;
    --run-id)
      RUN_ID="${2:-}"
      shift 2
      ;;
    --runner)
      RUNNER="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

cmd=(
  cargo run -p fr-conformance --bin adversarial_triage --
  --manifest "$MANIFEST"
  --output-root "$OUTPUT_ROOT"
  --run-id "$RUN_ID"
)

if [[ "$RUNNER" == "rch" ]]; then
  cmd=(~/.local/bin/rch exec -- "${cmd[@]}")
fi

echo "runner=${RUNNER}"
printf 'cmd='
printf '%q ' "${cmd[@]}"
echo

"${cmd[@]}"
