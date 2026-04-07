#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/benchmark_gate.sh [options]

Options:
  --port <port>             Server port to use for benchmark runs (default: 6399)
  --bind <addr>             Bind address for benchmark runs (default: 127.0.0.1)
  --mode <mode>             FrankenRedis runtime mode: strict|hardened (default: hardened)
  --requests <count>        Requests per workload (default: 100000)
  --clients <count>         Concurrent benchmark clients (default: 50)
  --datasize <bytes>        Payload size for write workloads (default: 3)
  --keyspace <count>        Keyspace size for benchmark runs (default: 10000)
  --out-dir <path>          Output directory for gate artifacts
  --run-id <id>             Artifact subdirectory name (default: gate-<utc timestamp>)
  --baseline-prefix <name>  Use baselines/<name>_<workload>.json instead of auto-discovery
  --skip-build              Skip the Rust release build step
  --help                    Show this help text

Environment:
  FR_BENCH_THROUGHPUT_DROP_PCT  Allowed throughput drop percent before failure (default: 15)
  FR_BENCH_P99_REGRESSION_PCT   Allowed p99 latency increase percent before failure (default: 10)
  FR_BENCH_BUILD_RUNNER         auto|rch|local (default: auto)

Description:
  Builds FrankenRedis + fr-bench, runs the standard benchmark workload suite
  against a local FrankenRedis instance, compares each workload against the
  checked-in FrankenRedis baselines, writes a delta report under
  artifacts/benchmark/, and exits non-zero when throughput or p99 latency
  regress past the configured thresholds.

Notes:
  - The build step is offloaded with `rch exec -- ...` when available (or when
    `FR_BENCH_BUILD_RUNNER=rch`).
  - Baseline auto-discovery picks the newest matching
    `baselines/frankenredis_*_<workload>.json`.
  - Artifacts are preserved; this script does not delete its output directory.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
port=6399
bind_addr="127.0.0.1"
server_mode="hardened"
requests=100000
clients=50
datasize=3
keyspace=10000
out_dir="${FR_BENCH_OUT_DIR:-$repo_root/artifacts/benchmark}"
run_id="gate-$(date -u +%Y%m%dT%H%M%SZ)"
baseline_prefix="${FR_BENCH_BASELINE_PREFIX:-}"
skip_build=0

throughput_drop_pct="${FR_BENCH_THROUGHPUT_DROP_PCT:-15}"
p99_regression_pct="${FR_BENCH_P99_REGRESSION_PCT:-10}"
build_runner="${FR_BENCH_BUILD_RUNNER:-auto}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --port)
      port="$2"
      shift 2
      ;;
    --bind)
      bind_addr="$2"
      shift 2
      ;;
    --mode)
      server_mode="$2"
      shift 2
      ;;
    --requests)
      requests="$2"
      shift 2
      ;;
    --clients)
      clients="$2"
      shift 2
      ;;
    --datasize)
      datasize="$2"
      shift 2
      ;;
    --keyspace)
      keyspace="$2"
      shift 2
      ;;
    --out-dir)
      out_dir="$2"
      shift 2
      ;;
    --run-id)
      run_id="$2"
      shift 2
      ;;
    --baseline-prefix)
      baseline_prefix="$2"
      shift 2
      ;;
    --skip-build)
      skip_build=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ "$server_mode" != "strict" && "$server_mode" != "hardened" ]]; then
  echo "error: --mode must be strict or hardened" >&2
  exit 1
fi

artifact_root="$out_dir/$run_id"
raw_dir="$artifact_root/raw"
candidate_dir="$artifact_root/candidate"
compare_dir="$artifact_root/compare"
log_dir="$artifact_root/logs"
manifest_path="$artifact_root/workloads.tsv"
report_path="$artifact_root/gate_report.json"
summary_path="$artifact_root/summary.md"

mkdir -p "$raw_dir" "$candidate_dir" "$compare_dir" "$log_dir"
: >"$manifest_path"

fr_bin="$repo_root/target/release/frankenredis"
bench_bin="$repo_root/target/release/fr-bench"
repo_redis_cli="$repo_root/legacy_redis_code/redis/src/redis-cli"
if [[ -x "$repo_redis_cli" ]]; then
  redis_cli="$repo_redis_cli"
elif command -v redis-cli >/dev/null 2>&1; then
  redis_cli="$(command -v redis-cli)"
else
  echo "error: redis-cli not found in legacy_redis_code or PATH" >&2
  exit 1
fi
server_pid=""
server_log=""

cleanup() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    "$redis_cli" -h "$bind_addr" -p "$port" SHUTDOWN NOSAVE >/dev/null 2>&1 || true
    kill "$server_pid" >/dev/null 2>&1 || true
    wait "$server_pid" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

require_executable() {
  local path="$1"
  if [[ ! -x "$path" ]]; then
    echo "error: required executable not found: $path" >&2
    exit 1
  fi
}

build_release_binaries() {
  if [[ "$skip_build" -eq 1 ]]; then
    return
  fi

  local cmd=(env CARGO_TARGET_DIR="$repo_root/target" cargo build --release -p fr-server -p fr-bench)
  case "$build_runner" in
    auto)
      if command -v rch >/dev/null 2>&1; then
        (
          cd "$repo_root"
          rch exec -- "${cmd[@]}"
        )
      else
        (
          cd "$repo_root"
          "${cmd[@]}"
        )
      fi
      ;;
    rch)
      if ! command -v rch >/dev/null 2>&1; then
        echo "error: FR_BENCH_BUILD_RUNNER=rch but rch is not available" >&2
        exit 1
      fi
      (
        cd "$repo_root"
        rch exec -- "${cmd[@]}"
      )
      ;;
    local)
      (
        cd "$repo_root"
        "${cmd[@]}"
      )
      ;;
    *)
      echo "error: FR_BENCH_BUILD_RUNNER must be auto, rch, or local" >&2
      exit 1
      ;;
  esac
}

wait_for_ping() {
  local attempt
  for attempt in $(seq 1 100); do
    if "$redis_cli" -h "$bind_addr" -p "$port" PING >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "error: server on ${bind_addr}:${port} did not become ready" >&2
  if [[ -n "$server_log" ]] && [[ -f "$server_log" ]]; then
    echo "server log:" >&2
    tail -n 80 "$server_log" >&2 || true
  fi
  exit 1
}

start_frankenredis() {
  server_log="$log_dir/frankenredis.log"
  "$fr_bin" --bind "$bind_addr" --port "$port" --mode "$server_mode" \
    >"$server_log" 2>&1 &
  server_pid="$!"
  wait_for_ping
}

resolve_baseline_path() {
  local workload_name="$1"
  if [[ -n "$baseline_prefix" ]]; then
    local explicit="$repo_root/baselines/${baseline_prefix}_${workload_name}.json"
    if [[ ! -f "$explicit" ]]; then
      echo "error: baseline file not found: $explicit" >&2
      exit 1
    fi
    printf '%s\n' "$explicit"
    return
  fi

  python3 - "$repo_root/baselines" "$workload_name" <<'PY'
import pathlib
import sys

baseline_dir = pathlib.Path(sys.argv[1])
workload = sys.argv[2]
matches = [path for path in baseline_dir.glob(f"frankenredis_*_{workload}.json") if path.is_file()]
if not matches:
    raise SystemExit(f"error: no FrankenRedis baseline found for workload {workload!r}")
matches.sort(key=lambda path: (path.stat().st_mtime, path.name))
print(matches[-1])
PY
}

run_workload() {
  local workload_name="$1"
  local bench_workload="$2"
  local pipeline="$3"
  local read_percent="$4"

  local baseline_path
  baseline_path="$(resolve_baseline_path "$workload_name")"
  local raw_path="$raw_dir/${workload_name}.json"
  local candidate_path="$candidate_dir/${workload_name}.json"
  local compare_json_path="$compare_dir/${workload_name}.json"
  local compare_text_path="$compare_dir/${workload_name}.txt"

  "$redis_cli" -h "$bind_addr" -p "$port" FLUSHALL >/dev/null

  local cmd=(
    "$bench_bin"
    --host "$bind_addr"
    --port "$port"
    --workload "$bench_workload"
    --requests "$requests"
    --clients "$clients"
    --pipeline "$pipeline"
    --keyspace "$keyspace"
    --datasize "$datasize"
    --read-percent "$read_percent"
    --json-out "$raw_path"
  )

  echo "benchmark gate: workload=${workload_name} baseline=${baseline_path}"
  "${cmd[@]}"
  python3 - "$raw_path" "$candidate_path" "$workload_name" <<'PY'
from __future__ import annotations

import json
import sys

raw_path, candidate_path, workload_name = sys.argv[1:]

with open(raw_path, encoding="utf-8") as handle:
    raw = json.load(handle)

latency = raw["latency_us"]
candidate = {
    "schema_version": "frankenredis_baseline/v1",
    "server": "frankenredis",
    "server_version": "candidate",
    "workload": workload_name,
    "host": raw["host"],
    "port": raw["port"],
    "clients": raw["clients"],
    "pipeline": raw["pipeline"],
    "keyspace": raw["keyspace"],
    "datasize": raw["datasize"],
    "read_percent": raw["read_percent"],
    "generated_at_ms": raw["generated_at_ms"],
    "total_requests": raw["requests"],
    "total_time_sec": raw["total_time_ms"] / 1000.0,
    "ops_sec": raw["ops_per_sec"],
    "p50_us": latency["p50"],
    "p95_us": latency["p95"],
    "p99_us": latency["p99"],
    "p999_us": latency["p999"],
    "bytes_sent": raw["bytes_sent"],
    "bytes_received": raw["bytes_received"],
    "raw_report": raw,
}

with open(candidate_path, "w", encoding="utf-8") as handle:
    json.dump(candidate, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
  python3 "$repo_root/scripts/compare_baselines.py" --json "$baseline_path" "$candidate_path" \
    >"$compare_json_path"
  python3 "$repo_root/scripts/compare_baselines.py" "$baseline_path" "$candidate_path" \
    >"$compare_text_path"
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$workload_name" "$baseline_path" "$raw_path" "$candidate_path" "$compare_json_path" "$compare_text_path" \
    >>"$manifest_path"
}

write_gate_report() {
  FR_GATE_BIND="$bind_addr" \
    FR_GATE_PORT="$port" \
    FR_GATE_MODE="$server_mode" \
    FR_GATE_REQUESTS="$requests" \
    FR_GATE_CLIENTS="$clients" \
    FR_GATE_DATASIZE="$datasize" \
    FR_GATE_KEYSPACE="$keyspace" \
    python3 - "$manifest_path" "$report_path" "$summary_path" "$throughput_drop_pct" "$p99_regression_pct" <<'PY'
from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

manifest_path = Path(sys.argv[1])
report_path = Path(sys.argv[2])
summary_path = Path(sys.argv[3])
throughput_drop_pct = float(sys.argv[4])
p99_regression_pct = float(sys.argv[5])

workloads: list[dict[str, object]] = []
failing_workloads: list[str] = []

for line in manifest_path.read_text(encoding="utf-8").splitlines():
    if not line.strip():
        continue
    (
        workload_name,
        baseline_path,
        raw_path,
        candidate_path,
        compare_json_path,
        compare_text_path,
    ) = line.split("\t")
    comparison = json.loads(Path(compare_json_path).read_text(encoding="utf-8"))
    ops_delta_pct = float(comparison["metrics"]["ops_sec"]["delta_pct"])
    p99_delta_pct = float(comparison["metrics"]["p99_us"]["delta_pct"])
    failures: list[str] = []
    if ops_delta_pct < (-throughput_drop_pct):
        failures.append(
            f"throughput dropped {abs(ops_delta_pct):.2f}% (allowed {throughput_drop_pct:.2f}%)"
        )
    if p99_delta_pct > p99_regression_pct:
        failures.append(
            f"p99 latency regressed {p99_delta_pct:.2f}% (allowed {p99_regression_pct:.2f}%)"
        )
    status = "pass" if not failures else "fail"
    if failures:
        failing_workloads.append(workload_name)
    workloads.append(
        {
            "workload": workload_name,
            "status": status,
            "baseline_path": baseline_path,
            "raw_report_path": raw_path,
            "candidate_path": candidate_path,
            "comparison_path": compare_json_path,
            "comparison_text_path": compare_text_path,
            "ops_sec_delta_pct": ops_delta_pct,
            "p99_delta_pct": p99_delta_pct,
            "failures": failures,
            "comparison": comparison,
        }
    )

passed = not failing_workloads
report = {
    "schema_version": "frankenredis_benchmark_gate/v1",
    "generated_at_ms": int(time.time() * 1000),
    "passed": passed,
    "thresholds": {
        "throughput_drop_pct": throughput_drop_pct,
        "p99_regression_pct": p99_regression_pct,
    },
    "config": {
        "bind": os.environ["FR_GATE_BIND"],
        "port": int(os.environ["FR_GATE_PORT"]),
        "mode": os.environ["FR_GATE_MODE"],
        "requests": int(os.environ["FR_GATE_REQUESTS"]),
        "clients": int(os.environ["FR_GATE_CLIENTS"]),
        "datasize": int(os.environ["FR_GATE_DATASIZE"]),
        "keyspace": int(os.environ["FR_GATE_KEYSPACE"]),
    },
    "workloads": workloads,
    "failing_workloads": failing_workloads,
}
report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")

summary_lines = [
    "# Benchmark Regression Gate",
    "",
    f"- status: {'PASS' if passed else 'FAIL'}",
    f"- throughput_drop_pct threshold: {throughput_drop_pct:.2f}",
    f"- p99_regression_pct threshold: {p99_regression_pct:.2f}",
    f"- report: `{report_path}`",
    "",
    "| workload | status | ops/sec delta | p99 delta | baseline | candidate |",
    "| --- | --- | ---: | ---: | --- | --- |",
]

for workload in workloads:
    summary_lines.append(
        "| {workload} | {status} | {ops:+.2f}% | {p99:+.2f}% | `{baseline}` | `{candidate}` |".format(
            workload=workload["workload"],
            status=workload["status"],
            ops=workload["ops_sec_delta_pct"],
            p99=workload["p99_delta_pct"],
            baseline=workload["baseline_path"],
            candidate=workload["candidate_path"],
        )
    )
    failures = workload["failures"]
    if failures:
        for failure in failures:
            summary_lines.append(f"  - {workload['workload']}: {failure}")

summary_path.write_text("\n".join(summary_lines) + "\n", encoding="utf-8")
print("PASS" if passed else "FAIL")
PY
}

build_release_binaries
require_executable "$fr_bin"
require_executable "$bench_bin"
require_executable "$redis_cli"

start_frankenredis
run_workload "set" "set" 1 0
run_workload "get" "get" 1 100
run_workload "mixed" "mixed" 1 50
run_workload "pipeline16" "set" 16 0
run_workload "incr" "incr" 1 0

gate_status="$(write_gate_report)"
echo "wrote $report_path"
echo "wrote $summary_path"
echo "artifacts: $artifact_root"
cat "$summary_path"

if [[ "$gate_status" != "PASS" ]]; then
  exit 1
fi
