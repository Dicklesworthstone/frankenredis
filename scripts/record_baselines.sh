#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/record_baselines.sh [options]

Options:
  --port <port>           Server port to use for benchmark runs (default: 6399)
  --bind <addr>           Bind address for benchmark runs (default: 127.0.0.1)
  --requests <count>      Requests per workload (default: 100000)
  --clients <count>       Concurrent benchmark clients (default: 50)
  --datasize <bytes>      Payload size for write workloads (default: 3)
  --keyspace <count>      Keyspace size for benchmark runs (default: 10000)
  --out-dir <path>        Output directory for normalized baselines
  --skip-build            Skip the Rust release build step
  --help                  Show this help text

Description:
  Builds FrankenRedis and fr-bench in release mode, runs the standard workload
  suite against FrankenRedis and legacy Redis, and writes normalized JSON
  baseline artifacts into baselines/.

Notes:
  - When `rch` is available, the Rust build is offloaded via:
      rch exec -- env CARGO_TARGET_DIR="$REPO_ROOT/target" cargo build --release -p fr-server -p fr-bench
  - Legacy Redis is expected at `legacy_redis_code/redis/src/redis-server`.
  - Output files use the normalized schema `frankenredis_baseline/v1`.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
port=6399
bind_addr="127.0.0.1"
requests=100000
clients=50
datasize=3
keyspace=10000
out_dir="$repo_root/baselines"
skip_build=0

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

fr_version="v0.1.0"
legacy_version="$(
  "$repo_root/legacy_redis_code/redis/src/redis-server" --version \
    | sed -n 's/.* v=\([0-9.][0-9.]*\).*/\1/p'
)"
if [[ -z "$legacy_version" ]]; then
  legacy_version="unknown"
fi

fr_bin="$repo_root/target/release/frankenredis"
bench_bin="$repo_root/target/release/fr-bench"
legacy_bin="$repo_root/legacy_redis_code/redis/src/redis-server"
redis_cli="$repo_root/legacy_redis_code/redis/src/redis-cli"
server_mode="${FRANKENREDIS_MODE:-hardened}"

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/fr-baselines.XXXXXX")"
server_pid=""
server_log=""

cleanup() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    "$redis_cli" -h "$bind_addr" -p "$port" SHUTDOWN NOSAVE >/dev/null 2>&1 || true
    kill "$server_pid" >/dev/null 2>&1 || true
    wait "$server_pid" >/dev/null 2>&1 || true
  fi
  rm -rf "$tmp_dir"
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

  if command -v rch >/dev/null 2>&1; then
    (
      cd "$repo_root"
      rch exec -- env CARGO_TARGET_DIR="$repo_root/target" \
        cargo build --release -p fr-server -p fr-bench
    )
  else
    (
      cd "$repo_root"
      cargo build --release -p fr-server -p fr-bench
    )
  fi
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

stop_server() {
  if [[ -z "$server_pid" ]]; then
    return
  fi
  "$redis_cli" -h "$bind_addr" -p "$port" SHUTDOWN NOSAVE >/dev/null 2>&1 || true
  kill "$server_pid" >/dev/null 2>&1 || true
  wait "$server_pid" >/dev/null 2>&1 || true
  server_pid=""
}

start_frankenredis() {
  stop_server
  server_log="$tmp_dir/frankenredis.log"
  "$fr_bin" --bind "$bind_addr" --port "$port" --mode "$server_mode" \
    >"$server_log" 2>&1 &
  server_pid="$!"
  wait_for_ping
}

start_legacy_redis() {
  stop_server
  mkdir -p "$tmp_dir/legacy"
  server_log="$tmp_dir/legacy-redis.log"
  "$legacy_bin" \
    --bind "$bind_addr" \
    --port "$port" \
    --save "" \
    --appendonly no \
    --dir "$tmp_dir/legacy" \
    >"$server_log" 2>&1 &
  server_pid="$!"
  wait_for_ping
}

normalize_report() {
  local raw_path="$1"
  local final_path="$2"
  local server_name="$3"
  local version="$4"
  local workload_name="$5"

  python3 - "$raw_path" "$final_path" "$server_name" "$version" "$workload_name" <<'PY'
import json
import sys

raw_path, final_path, server_name, version, workload_name = sys.argv[1:]

with open(raw_path, encoding="utf-8") as handle:
    raw = json.load(handle)

latency = raw["latency_us"]
baseline = {
    "schema_version": "frankenredis_baseline/v1",
    "server": server_name,
    "server_version": version,
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

with open(final_path, "w", encoding="utf-8") as handle:
    json.dump(baseline, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
}

run_benchmark() {
  local server_name="$1"
  local version="$2"
  local workload_name="$3"
  local bench_workload="$4"
  local pipeline="$5"
  local read_percent="$6"

  local raw_path="$tmp_dir/${server_name}_${workload_name}_raw.json"
  local final_path="$out_dir/${server_name}_${version}_${workload_name}.json"

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

  echo "benchmark: server=${server_name} workload=${workload_name} pipeline=${pipeline}"
  "${cmd[@]}"
  normalize_report "$raw_path" "$final_path" "$server_name" "$version" "$workload_name"
  echo "wrote $final_path"
}

mkdir -p "$out_dir"
build_release_binaries
require_executable "$fr_bin"
require_executable "$bench_bin"
require_executable "$legacy_bin"
require_executable "$redis_cli"

start_frankenredis
run_benchmark "frankenredis" "$fr_version" "set" "set" 1 0
run_benchmark "frankenredis" "$fr_version" "get" "get" 1 100
run_benchmark "frankenredis" "$fr_version" "mixed" "mixed" 1 50
run_benchmark "frankenredis" "$fr_version" "pipeline16" "set" 16 0
run_benchmark "frankenredis" "$fr_version" "incr" "incr" 1 0
stop_server

start_legacy_redis
run_benchmark "redis" "$legacy_version" "set" "set" 1 0
run_benchmark "redis" "$legacy_version" "get" "get" 1 100
run_benchmark "redis" "$legacy_version" "mixed" "mixed" 1 50
run_benchmark "redis" "$legacy_version" "pipeline16" "set" 16 0
run_benchmark "redis" "$legacy_version" "incr" "incr" 1 0
stop_server

echo "baseline capture complete"
