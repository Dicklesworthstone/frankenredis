#!/usr/bin/env bash
set -euo pipefail

: "${OUT:?OUT is required}"
: "${SERVER:?SERVER is required}"
: "${BENCH:?BENCH is required}"
: "${PORT:?PORT is required}"
: "${WORKLOAD:?WORKLOAD is required}"
: "${REQUESTS:?REQUESTS is required}"
: "${JSON_OUT:?JSON_OUT is required}"
: "${KEY_PREFIX:?KEY_PREFIX is required}"

log="$OUT/server-${WORKLOAD}-${PORT}.log"
"$SERVER" --port "$PORT" --mode strict >"$log" 2>&1 &
pid=$!

cleanup() {
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}
trap cleanup EXIT

for _ in {1..100}; do
  if grep -q ready "$log"; then
    break
  fi
  kill -0 "$pid" 2>/dev/null
  sleep 0.05
done

grep -q ready "$log"

"$BENCH" \
  --host 127.0.0.1 \
  --port "$PORT" \
  --clients 50 \
  --requests "$REQUESTS" \
  --pipeline 16 \
  --workload "$WORKLOAD" \
  --keyspace 10000 \
  --datasize 3 \
  --json-out "$JSON_OUT" \
  --key-prefix "$KEY_PREFIX" \
  >/dev/null
