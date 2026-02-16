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
COVERAGE_SUMMARY="${RUN_ROOT}/coverage_summary.json"
FAILURE_ENVELOPE="${RUN_ROOT}/failure_envelope.json"

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

declare -a SUITE_NAMES=("core_errors" "core_strings" "fr_p2c_001_eventloop_journey" "protocol_negative")
declare -a SUITE_MODES=("command" "command" "command" "protocol")
declare -a SUITE_FIXTURES=("core_errors.json" "core_strings.json" "fr_p2c_001_eventloop_journey.json" "protocol_negative.json")

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
    --log-root "$LIVE_LOG_ROOT" --json-out "$suite_report" --run-id "$RUN_ID"
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
- \`coverage_summary.json\`: aggregated pass-rate and reason-code budget input.
- \`failure_envelope.json\`: per-failure envelope with replay pointers + deterministic artifact index.
- \`replay_failed.sh\`: deterministic replay commands for failed suites.

## Re-run

\`\`\`bash
./scripts/run_live_oracle_diff.sh --host ${HOST} --port ${PORT} --run-id ${RUN_ID}
\`\`\`
EOF

python3 - "$STATUS_TSV" "$RUN_ID" "$HOST" "$PORT" "$RUNNER" "$RUN_ROOT" "$README_PATH" "$REPLAY_SCRIPT" "$COVERAGE_SUMMARY" "$FAILURE_ENVELOPE" <<'PY'
import collections
import csv
import json
import os
import sys

status_tsv, run_id, host, port, runner, run_root, readme_path, replay_script, out_path, failure_envelope_path = sys.argv[1:]

suite_rows = []
reason_counts = collections.Counter()
total_case_failures = 0
flake_suspects = []
hard_fail_suites = []
packet_totals = collections.defaultdict(lambda: {"total_suites": 0, "passed_suites": 0})
failure_envelope_rows = []
artifact_index = collections.defaultdict(list)


def packet_id_for_fixture(fixture_name: str) -> str:
    if fixture_name == "fr_p2c_001_eventloop_journey.json":
        return "FR-P2C-001"
    if fixture_name == "protocol_negative.json":
        return "FR-P2C-002"
    if fixture_name == "persist_replay.json":
        return "FR-P2C-005"
    return "FR-P2C-003"

with open(status_tsv, newline="", encoding="utf-8") as fh:
    reader = csv.DictReader(fh, delimiter="\t")
    for row in reader:
        if not row.get("suite"):
            continue
        exit_code = int(row.get("exit_code", "1"))
        report_path = row.get("report_json", "")
        report = {}
        if report_path and os.path.exists(report_path):
            try:
                with open(report_path, encoding="utf-8") as report_fh:
                    report = json.load(report_fh)
            except Exception as exc:  # noqa: BLE001
                report = {
                    "status": "execution_error",
                    "run_error": f"report_parse_error:{exc}",
                }

        report_status = report.get("status", "missing_report")
        failed_count = int(report.get("failed_count", 0) or 0)
        report_pass_rate = float(report.get("pass_rate", 0.0) or 0.0)
        run_error = report.get("run_error")
        case_reason_counts = report.get("reason_code_counts") or {}
        packet_id = packet_id_for_fixture(row.get("fixture", ""))
        for reason_code, count in case_reason_counts.items():
            reason_counts[str(reason_code)] += int(count)

        for failure in report.get("failures") or []:
            case_name = str(failure.get("case_name") or "")
            reason_code = failure.get("reason_code")
            replay_cmd = failure.get("replay_cmd")
            artifact_refs = [
                str(artifact_ref)
                for artifact_ref in (failure.get("artifact_refs") or [])
                if str(artifact_ref).strip()
            ]
            envelope_row = {
                "suite": row["suite"],
                "fixture": row.get("fixture"),
                "packet_id": packet_id,
                "case_name": case_name,
                "reason_code": reason_code,
                "detail": failure.get("detail"),
                "replay_cmd": replay_cmd,
                "artifact_refs": artifact_refs,
                "report_json": report_path,
                "stdout_log": row.get("stdout_log"),
                "live_log_root": report.get("live_log_root"),
            }
            failure_envelope_rows.append(envelope_row)
            for artifact_ref in artifact_refs:
                artifact_index[artifact_ref].append(
                    {
                        "suite": row["suite"],
                        "case_name": case_name,
                        "reason_code": reason_code,
                        "replay_cmd": replay_cmd,
                    }
                )

        total_case_failures += failed_count
        packet_totals[packet_id]["total_suites"] += 1
        if exit_code == 0:
            packet_totals[packet_id]["passed_suites"] += 1
        if exit_code != 0:
            if report_status == "execution_error":
                hard_fail_suites.append(row["suite"])
            else:
                flake_suspects.append(row["suite"])

        suite_rows.append(
            {
                "suite": row["suite"],
                "mode": row.get("mode"),
                "fixture": row.get("fixture"),
                "packet_id": packet_id,
                "exit_code": exit_code,
                "report_json": report_path,
                "stdout_log": row.get("stdout_log"),
                "report_status": report_status,
                "failed_count": failed_count,
                "pass_rate": round(report_pass_rate, 4),
                "run_error": run_error,
                "reason_code_counts": case_reason_counts,
            }
        )

total_suites = len(suite_rows)
passed_suites = sum(1 for row in suite_rows if row["exit_code"] == 0)
failed_suites = total_suites - passed_suites
pass_rate = round((passed_suites / total_suites) if total_suites else 0.0, 4)
packet_family_pass_rates = []
for packet_id, totals in sorted(packet_totals.items()):
    packet_total = totals["total_suites"]
    packet_passed = totals["passed_suites"]
    packet_failed = packet_total - packet_passed
    packet_pass_rate = round((packet_passed / packet_total) if packet_total else 0.0, 4)
    packet_family_pass_rates.append(
        {
            "packet_id": packet_id,
            "total_suites": packet_total,
            "passed_suites": packet_passed,
            "failed_suites": packet_failed,
            "pass_rate": packet_pass_rate,
        }
    )

summary = {
    "schema_version": "live_oracle_coverage_summary/v1",
    "run_id": run_id,
    "host": host,
    "port": int(port),
    "runner": runner,
    "run_root": run_root,
    "status_tsv": status_tsv,
    "readme_path": readme_path,
    "replay_script": replay_script,
    "failure_envelope": failure_envelope_path,
    "total_suites": total_suites,
    "passed_suites": passed_suites,
    "failed_suites": failed_suites,
    "pass_rate": pass_rate,
    "total_case_failures": total_case_failures,
    "packet_family_pass_rates": packet_family_pass_rates,
    "flake_suspect_suites": sorted(set(flake_suspects)),
    "hard_fail_suites": sorted(set(hard_fail_suites)),
    "primary_reason_codes": [
        {"reason_code": reason_code, "count": count}
        for reason_code, count in sorted(
            reason_counts.items(),
            key=lambda item: (-item[1], item[0]),
        )
    ],
    "suite_results": suite_rows,
}

with open(out_path, "w", encoding="utf-8") as out_fh:
    json.dump(summary, out_fh, indent=2)
    out_fh.write("\n")

failure_envelope_rows.sort(key=lambda row: (row["suite"], row["case_name"]))
deterministic_artifact_index = []
for artifact_ref in sorted(artifact_index):
    entries = sorted(
        artifact_index[artifact_ref],
        key=lambda entry: (entry["suite"], entry["case_name"]),
    )
    deterministic_artifact_index.append(
        {
            "artifact_ref": artifact_ref,
            "failure_count": len(entries),
            "failures": entries,
        }
    )

failure_envelope = {
    "schema_version": "live_oracle_failure_envelope/v1",
    "run_id": run_id,
    "run_root": run_root,
    "total_failures": len(failure_envelope_rows),
    "failures": failure_envelope_rows,
    "artifact_index": deterministic_artifact_index,
}
with open(failure_envelope_path, "w", encoding="utf-8") as out_fh:
    json.dump(failure_envelope, out_fh, indent=2)
    out_fh.write("\n")
PY

echo "coverage_summary: ${COVERAGE_SUMMARY}"
cat "$COVERAGE_SUMMARY"
echo "failure_envelope: ${FAILURE_ENVELOPE}"

if ((FAILED_COUNT > 0)); then
  echo "live oracle diffs failed (${FAILED_COUNT}/${TOTAL_COUNT}); bundle: ${RUN_ROOT}"
  exit 1
fi

echo "live oracle diffs passed (${TOTAL_COUNT}/${TOTAL_COUNT}); bundle: ${RUN_ROOT}"
