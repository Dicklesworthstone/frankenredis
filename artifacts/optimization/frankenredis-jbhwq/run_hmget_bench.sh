#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 3 ]; then
  printf 'usage: %s <name> <server-binary> <port>\n' "$0" >&2
  exit 2
fi

name="$1"
server="$2"
port="$3"
out="${OUT:-artifacts/optimization/frankenredis-jbhwq}"
redis_benchmark="${REDIS_BENCHMARK:-legacy_redis_code/redis/src/redis-benchmark}"
requests="${REQUESTS:-300000}"
field_count="${FIELD_COUNT:-3}"

mkdir -p "$out"
log="$out/${name}-hyperfine-server-${port}-$$.log"

"$server" --bind 127.0.0.1 --port "$port" --mode strict >"$log" 2>&1 &
pid="$!"

cleanup() {
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}
trap cleanup EXIT

for _ in {1..100}; do
  if grep -q ready "$log"; then
    break
  fi
  kill -0 "$pid" 2>/dev/null || exit 1
  sleep 0.05
done
grep -q ready "$log"

if [ "$field_count" -lt 1 ]; then
  printf 'FIELD_COUNT must be >= 1\n' >&2
  exit 2
fi

hset_args=()
hmget_args=()
for index in $(seq 1 "$field_count"); do
  hset_args+=("field${index}" "value${index}")
  hmget_args+=("field${index}")
done

"$redis_benchmark" -h 127.0.0.1 -p "$port" -c 1 -n 1 \
  HSET jbhwq:hash "${hset_args[@]}" >"$out/${name}-prefill-last.txt"

"$redis_benchmark" -h 127.0.0.1 -p "$port" -c 50 -n "$requests" -P 16 --csv \
  HMGET jbhwq:hash "${hmget_args[@]}" >"$out/${name}-redis-benchmark-last.txt"
