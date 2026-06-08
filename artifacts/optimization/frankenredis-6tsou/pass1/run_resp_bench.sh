#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <mode> <port> <json-out> [requests] [key-prefix]" >&2
  exit 2
fi

mode="$1"
port="$2"
json_out="$3"
requests="${4:-300000}"
key_prefix="${5:-fr6tsou-${mode}}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
server="$repo_root/target-orange-6tsou-pass1/release-perf/frankenredis"
driver="$repo_root/artifacts/optimization/frankenredis-6tsou/pass1/resp_workload.py"
log="$repo_root/artifacts/optimization/frankenredis-6tsou/pass1/server-${mode}-${port}.log"

"$server" --bind 127.0.0.1 --port "$port" >"$log" 2>&1 &
pid="$!"
trap 'kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true' EXIT
sleep 0.25

python3 "$driver" \
  --port "$port" \
  --mode "$mode" \
  --requests "$requests" \
  --clients 50 \
  --pipeline 16 \
  --keyspace 10000 \
  --datasize 3 \
  --key-prefix "$key_prefix" \
  --json-out "$json_out" >/dev/null
