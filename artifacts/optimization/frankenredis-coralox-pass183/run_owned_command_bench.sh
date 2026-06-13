#!/usr/bin/env bash
set -euo pipefail

server_kind="${1:?server kind: fr|redis}"
port="${2:?port}"
requests="${3:?requests}"
seed="${4:?seed}"
log_path="${5:?log path}"
tests="${6:?redis-benchmark tests}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
fr_server="$repo_root/target-coralox-pass183-baseline/release-perf/frankenredis"
redis_server="$repo_root/legacy_redis_code/redis/src/redis-server"
redis_cli="$repo_root/legacy_redis_code/redis/src/redis-cli"
redis_benchmark="$repo_root/legacy_redis_code/redis/src/redis-benchmark"

server_pid=""
server_log="${log_path%.log}.server.log"
cleanup() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

case "$server_kind" in
  fr)
    "$fr_server" --port "$port" >"$server_log" 2>&1 &
    ;;
  redis)
    "$redis_server" --port "$port" --save "" --appendonly no --daemonize no >"$server_log" 2>&1 &
    ;;
  *)
    echo "unknown server kind: $server_kind" >&2
    exit 64
    ;;
esac
server_pid="$!"

for _ in $(seq 1 100); do
  if "$redis_cli" -p "$port" PING >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done

"$redis_cli" -p "$port" PING >/dev/null
"$redis_cli" -p "$port" FLUSHALL >/dev/null

"$redis_benchmark" -p "$port" -t "$tests" -n "$requests" -c 50 -P 16 -r 100000 --seed "$seed" --csv | tee "$log_path"
