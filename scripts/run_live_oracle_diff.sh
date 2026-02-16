#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/run_live_oracle_diff.sh [--host <host>] [--port <port>] [--output-root <dir>] [--run-id <id>]
  ./scripts/run_live_oracle_diff.sh [host] [port]

Description:
  Deterministic local/CI orchestrator for live Redis differential E2E suites.
  It creates a self-contained failure bundle with per-suite logs, JSON reports,
  replay commands, and command trace artifacts.
USAGE
}

HOST="127.0.0.1"
PORT="6379"
OUTPUT_ROOT="${FR_E2E_OUTPUT_ROOT:-artifacts/e2e_orchestrator}"
RUN_ID="${FR_E2E_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
RUNNER="${FR_E2E_RUNNER:-local}"

POSITIONAL=()
while (($# > 0)); do
  case "$1" in
    --host)
      HOST="${2:-}"
      shift 2
      ;;
    --port)
      PORT="${2:-}"
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
    -h|--help)
      usage
      exit 0
      ;;
    *)
      POSITIONAL+=("$1")
      shift
      ;;
  esac
done

if ((${#POSITIONAL[@]} > 0)); then
  HOST="${POSITIONAL[0]}"
fi
if ((${#POSITIONAL[@]} > 1)); then
  PORT="${POSITIONAL[1]}"
fi

RUN_ROOT="${OUTPUT_ROOT%/}/${RUN_ID}"
SUITES_ROOT="${RUN_ROOT}/suites"
LIVE_LOG_ROOT="${RUN_ROOT}/live_logs"
TRACE_LOG="${RUN_ROOT}/command_trace.log"
STATUS_TSV="${RUN_ROOT}/suite_status.tsv"
REPLAY_SCRIPT="${RUN_ROOT}/replay_failed.sh"
README_PATH="${RUN_ROOT}/README.md"

mkdir -p "$SUITES_ROOT" "$LIVE_LOG_ROOT"
: > "$TRACE_LOG"
printf "suite\tmode\tfixture\texit_code\treport_json\tstdout_log\n" > "$STATUS_TSV"

cat > "$REPLAY_SCRIPT" <<'REPLAY'
#!/usr/bin/env bash
set -euo pipefail
REPLAY
chmod +x "$REPLAY_SCRIPT"

echo "Verifying live Redis endpoint ${HOST}:${PORT}"
redis-cli -h "$HOST" -p "$PORT" ping >/dev/null

declare -a SUITE_NAMES=("core_errors" "core_strings" "protocol_negative")
declare -a SUITE_MODES=("command" "command" "protocol")
declare -a SUITE_FIXTURES=("core_errors.json" "core_strings.json" "protocol_negative.json")

FAILED_COUNT=0
TOTAL_COUNT=0

for idx in "${!SUITE_NAMES[@]}"; do
  TOTAL_COUNT=$((TOTAL_COUNT + 1))
  suite_name="${SUITE_NAMES[$idx]}"
  mode="${SUITE_MODES[$idx]}"
  fixture="${SUITE_FIXTURES[$idx]}"

  suite_dir="${SUITES_ROOT}/${suite_name}"
  suite_log="${suite_dir}/stdout.log"
  suite_report="${suite_dir}/report.json"
  mkdir -p "$suite_dir"

  cmd=(
    cargo run -p fr-conformance --bin live_oracle_diff --
    --log-root "$LIVE_LOG_ROOT" --json-out "$suite_report"
    "$mode" "$fixture" "$HOST" "$PORT"
  )
  if [[ "$RUNNER" == "rch" ]]; then
    cmd=(~/.local/bin/rch exec -- "${cmd[@]}")
  fi

  {
    echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] suite=${suite_name}"
    echo "runner=${RUNNER}"
    printf 'cmd='
    printf '%q ' "${cmd[@]}"
    echo
  } >> "$TRACE_LOG"

  echo "running ${suite_name} (${mode} ${fixture})"
  set +e
  "${cmd[@]}" >"$suite_log" 2>&1
  exit_code=$?
  set -e

  printf "%s\t%s\t%s\t%d\t%s\t%s\n" \
    "$suite_name" "$mode" "$fixture" "$exit_code" "$suite_report" "$suite_log" >> "$STATUS_TSV"

  if ((exit_code != 0)); then
    FAILED_COUNT=$((FAILED_COUNT + 1))
    {
      printf '\n# %s\n' "$suite_name"
      printf '%q ' "${cmd[@]}"
      echo
    } >> "$REPLAY_SCRIPT"
    echo "failed: ${suite_name} (exit ${exit_code})"
  else
    echo "passed: ${suite_name}"
  fi
done

cat > "$README_PATH" <<EOF
# Live Oracle Diff Bundle

- run_id: \`${RUN_ID}\`
- host: \`${HOST}\`
- port: \`${PORT}\`
- runner: \`${RUNNER}\`
- total_suites: \`${TOTAL_COUNT}\`
- failed_suites: \`${FAILED_COUNT}\`

## Artifact Layout

- \`suite_status.tsv\`: machine-readable suite execution status.
- \`command_trace.log\`: exact command trace with timestamps.
- \`live_logs/\`: structured JSONL logs emitted by harness (\`live_log_root\`).
- \`suites/<suite>/stdout.log\`: captured command output.
- \`suites/<suite>/report.json\`: machine-readable diff report from \`live_oracle_diff --json-out\`.
- \`replay_failed.sh\`: deterministic replay commands for failed suites.

## Re-run

\`\`\`bash
./scripts/run_live_oracle_diff.sh --host ${HOST} --port ${PORT} --run-id ${RUN_ID}
\`\`\`
EOF

if ((FAILED_COUNT > 0)); then
  echo "live oracle diffs failed (${FAILED_COUNT}/${TOTAL_COUNT}); bundle: ${RUN_ROOT}"
  exit 1
fi

echo "live oracle diffs passed (${TOTAL_COUNT}/${TOTAL_COUNT}); bundle: ${RUN_ROOT}"
