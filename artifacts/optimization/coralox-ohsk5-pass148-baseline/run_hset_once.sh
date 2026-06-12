#!/usr/bin/env bash
set -euo pipefail

: "${ART:?missing artifact directory}"
: "${BENCH:?missing redis-benchmark path}"
: "${BIN:?missing frankenredis binary path}"
: "${CLIENTS:?missing client count}"
: "${KEYSPACE:?missing keyspace size}"
: "${NAME:?missing benchmark name}"
: "${N:?missing request count}"
: "${PIPE:?missing pipeline depth}"
: "${PORT:?missing server port}"

iter="${HYPERFINE_ITERATION:-manual}"

"$BIN" --port "$PORT" >"$ART/server-${NAME}-${iter}.log" 2>&1 &
pid=$!

cleanup() {
  kill -TERM "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}
trap cleanup EXIT

ready=0
for _ in $(seq 1 100); do
  if timeout 0.2 bash -c ": >/dev/tcp/127.0.0.1/$PORT" 2>/dev/null; then
    ready=1
    break
  fi
  sleep 0.05
done

if [ "$ready" != 1 ]; then
  echo "server did not accept connections on $PORT" >&2
  exit 1
fi

"$BENCH" -p "$PORT" -t hset -n "$N" -c "$CLIENTS" -P "$PIPE" -r "$KEYSPACE" -q \
  >"$ART/redis-benchmark-${NAME}-${iter}.log"
