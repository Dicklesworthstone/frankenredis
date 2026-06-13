#!/usr/bin/env bash
set -euo pipefail

kind="${1:?kind: fr|redis}"
port="${2:?port}"
requests="${3:?requests}"
seed="${4:?seed}"
log_path="${5:?log path}"
tests="${6:-lpush}"

repo="/data/projects/frankenredis"
cli="$repo/legacy_redis_code/redis/src/redis-cli"
bench="$repo/legacy_redis_code/redis/src/redis-benchmark"

case "$kind" in
  fr)
    "$repo/target-coralox-pass181-baseline/release-perf/frankenredis" \
      --bind 127.0.0.1 --port "$port" >>"$log_path" 2>&1 &
    ;;
  redis)
    "$repo/legacy_redis_code/redis/src/redis-server" \
      --bind 127.0.0.1 --port "$port" \
      --save "" --appendonly no --protected-mode no --loglevel warning \
      >>"$log_path" 2>&1 &
    ;;
  *)
    echo "unknown server kind: $kind" >&2
    exit 64
    ;;
esac

pid=$!
cleanup() {
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}
trap cleanup EXIT

ready=0
for _ in $(seq 1 200); do
  if "$cli" -p "$port" PING >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.01
done

if [[ "$ready" != 1 ]]; then
  echo "$kind server on port $port did not become ready" >&2
  exit 75
fi

"$cli" -p "$port" FLUSHALL >/dev/null
"$bench" -p "$port" -t "$tests" -n "$requests" -c 50 -P 16 -r 100000 --seed "$seed" --csv
