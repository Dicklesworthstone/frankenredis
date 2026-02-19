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
RUN_SEED="${FR_E2E_SEED:-424242}"

if command -v sha256sum >/dev/null 2>&1; then
  RUN_FINGERPRINT="$(printf '%s' "${RUN_ID}|${HOST}|${PORT}|${RUNNER}|${RUN_SEED}" | sha256sum | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  RUN_FINGERPRINT="$(printf '%s' "${RUN_ID}|${HOST}|${PORT}|${RUNNER}|${RUN_SEED}" | shasum -a 256 | awk '{print $1}')"
else
  RUN_FINGERPRINT="fingerprint-unavailable"
fi

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
REPLAY_ALL_SCRIPT="${RUN_ROOT}/replay_all.sh"
README_PATH="${RUN_ROOT}/README.md"
COVERAGE_SUMMARY="${RUN_ROOT}/coverage_summary.json"
FAILURE_ENVELOPE="${RUN_ROOT}/failure_envelope.json"

mkdir -p "$SUITES_ROOT" "$LIVE_LOG_ROOT"
: > "$TRACE_LOG"
printf "suite\tmode\tfixture\tscenario_class\texit_code\treport_json\tstdout_log\n" > "$STATUS_TSV"

cat > "$REPLAY_SCRIPT" <<'REPLAY'
#!/usr/bin/env bash
set -euo pipefail
REPLAY
chmod +x "$REPLAY_SCRIPT"

cat > "$REPLAY_ALL_SCRIPT" <<'REPLAY'
#!/usr/bin/env bash
set -euo pipefail
REPLAY
chmod +x "$REPLAY_ALL_SCRIPT"

echo "Verifying live Redis endpoint ${HOST}:${PORT}"
redis-cli -h "$HOST" -p "$PORT" ping >/dev/null

declare -a SUITE_NAMES=(
  "core_strings"
  "fr_p2c_001_eventloop_journey"
  "fr_p2c_003_dispatch_journey"
  "core_errors"
  "fr_p2c_002_protocol_negative"
)
declare -a SUITE_MODES=("command" "command" "command" "command" "protocol")
declare -a SUITE_FIXTURES=(
  "core_strings.json"
  "fr_p2c_001_eventloop_journey.json"
  "fr_p2c_003_dispatch_journey.json"
  "core_errors.json"
  "protocol_negative.json"
)
declare -a SUITE_CLASSES=("golden" "golden" "golden" "regression" "failure_injection")

FAILED_COUNT=0
TOTAL_COUNT=0

for idx in "${!SUITE_NAMES[@]}"; do
  TOTAL_COUNT=$((TOTAL_COUNT + 1))
  suite_name="${SUITE_NAMES[$idx]}"
  mode="${SUITE_MODES[$idx]}"
  fixture="${SUITE_FIXTURES[$idx]}"
  scenario_class="${SUITE_CLASSES[$idx]}"

  suite_dir="${SUITES_ROOT}/${suite_name}"
  suite_log="${suite_dir}/stdout.log"
  suite_report="${suite_dir}/report.json"
  mkdir -p "$suite_dir"

  cmd=(
    env FR_SEED="$RUN_SEED" cargo run -p fr-conformance --bin live_oracle_diff --
    --log-root "$LIVE_LOG_ROOT" --json-out "$suite_report" --run-id "$RUN_ID"
    "$mode" "$fixture" "$HOST" "$PORT"
  )
  if [[ "$RUNNER" == "rch" ]]; then
    cmd=(~/.local/bin/rch exec -- "${cmd[@]}")
  fi

  {
    echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] suite=${suite_name}"
    echo "runner=${RUNNER}"
    echo "scenario_class=${scenario_class}"
    echo "run_seed=${RUN_SEED}"
    echo "run_fingerprint=${RUN_FINGERPRINT}"
    printf 'cmd='
    printf '%q ' "${cmd[@]}"
    echo
  } >> "$TRACE_LOG"

  echo "running ${suite_name} (${mode} ${fixture})"
  set +e
  "${cmd[@]}" >"$suite_log" 2>&1
  exit_code=$?
  set -e

  printf "%s\t%s\t%s\t%s\t%d\t%s\t%s\n" \
    "$suite_name" "$mode" "$fixture" "$scenario_class" "$exit_code" "$suite_report" "$suite_log" >> "$STATUS_TSV"

  {
    printf '\n# %s (%s)\n' "$suite_name" "$scenario_class"
    printf '%q ' "${cmd[@]}"
    echo
  } >> "$REPLAY_ALL_SCRIPT"

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
- run_seed: \`${RUN_SEED}\`
- run_fingerprint: \`${RUN_FINGERPRINT}\`
- total_suites: \`${TOTAL_COUNT}\`
- failed_suites: \`${FAILED_COUNT}\`

## Artifact Layout

- \`suite_status.tsv\`: machine-readable suite execution status.
- \`command_trace.log\`: exact command trace with timestamps.
- \`live_logs/\`: structured JSONL logs emitted by harness (\`live_log_root\`).
- \`suites/<suite>/stdout.log\`: captured command output.
- \`suites/<suite>/report.json\`: machine-readable diff report from \`live_oracle_diff --json-out\`.
- \`coverage_summary.json\`: aggregated pass-rate and reason-code budget input.
- \`failure_envelope.json\`: per-failure envelope with replay pointers + deterministic artifact index.
- \`replay_all.sh\`: deterministic replay commands for the full suite matrix.
- \`replay_failed.sh\`: deterministic replay commands for failed suites.

## Scenario Matrix

- \`core_strings\` (golden)
- \`fr_p2c_001_eventloop_journey\` (golden)
- \`fr_p2c_003_dispatch_journey\` (golden)
- \`core_errors\` (regression)
- \`fr_p2c_002_protocol_negative\` (failure_injection, FR-P2C-002)

## Re-run

\`\`\`bash
FR_E2E_SEED=${RUN_SEED} ./scripts/run_live_oracle_diff.sh --host ${HOST} --port ${PORT} --run-id ${RUN_ID}
\`\`\`
EOF

summary_cmd=(
  cargo run -p fr-conformance --bin live_oracle_bundle_summarizer --
  --status-tsv "$STATUS_TSV"
  --run-id "$RUN_ID"
  --host "$HOST"
  --port "$PORT"
  --runner "$RUNNER"
  --run-root "$RUN_ROOT"
  --readme-path "$README_PATH"
  --replay-script "$REPLAY_SCRIPT"
  --replay-all-script "$REPLAY_ALL_SCRIPT"
  --coverage-summary-out "$COVERAGE_SUMMARY"
  --failure-envelope-out "$FAILURE_ENVELOPE"
  --run-seed "$RUN_SEED"
  --run-fingerprint "$RUN_FINGERPRINT"
)
if [[ "$RUNNER" == "rch" ]]; then
  summary_cmd=(~/.local/bin/rch exec -- "${summary_cmd[@]}")
fi
"${summary_cmd[@]}"

echo "coverage_summary: ${COVERAGE_SUMMARY}"
cat "$COVERAGE_SUMMARY"
echo "failure_envelope: ${FAILURE_ENVELOPE}"

if ((FAILED_COUNT > 0)); then
  echo "live oracle diffs failed (${FAILED_COUNT}/${TOTAL_COUNT}); bundle: ${RUN_ROOT}"
  exit 1
fi

echo "live oracle diffs passed (${TOTAL_COUNT}/${TOTAL_COUNT}); bundle: ${RUN_ROOT}"
