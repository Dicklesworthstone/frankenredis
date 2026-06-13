#!/usr/bin/env bash
set -euo pipefail

port="${1:?port}"
requests="${2:?requests}"
seed="${3:?seed}"
tests="${4:?redis-benchmark tests}"
summary_path="${5:?strace summary path}"
bench_log="${6:?benchmark log path}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
fr_server="$repo_root/target-coralox-pass183-baseline/release-perf/frankenredis"
redis_cli="$repo_root/legacy_redis_code/redis/src/redis-cli"
redis_benchmark="$repo_root/legacy_redis_code/redis/src/redis-benchmark"

server_pid=""
server_log="${summary_path%.txt}.server.log"
strace_log="${summary_path%.txt}.strace.log"
cleanup() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

strace -f -c -o "$summary_path" -e trace=network,desc,process \
  "$fr_server" --port "$port" >"$server_log" 2>"$strace_log" &
server_pid="$!"

for _ in $(seq 1 150); do
  if "$redis_cli" -p "$port" PING >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done

"$redis_cli" -p "$port" PING >/dev/null
"$redis_cli" -p "$port" FLUSHALL >/dev/null

"$redis_benchmark" -p "$port" -t "$tests" -n "$requests" -c 50 -P 16 -r 100000 --seed "$seed" --csv | tee "$bench_log"
