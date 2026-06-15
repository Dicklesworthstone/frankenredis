#!/usr/bin/env bash
set -euo pipefail

: "${SERVER:?SERVER is required}"
: "${BENCH:?BENCH is required}"
: "${PORT:?PORT is required}"
: "${OUT:?OUT is required}"
: "${LABEL:?LABEL is required}"
: "${DATASIZE:?DATASIZE is required}"
: "${REQUESTS:?REQUESTS is required}"
: "${JSON_OUT:?JSON_OUT is required}"
: "${KEY_PREFIX:?KEY_PREFIX is required}"

mkdir -p "$OUT" "$(dirname "$JSON_OUT")"

log="$OUT/server-${LABEL}-${DATASIZE}-${PORT}.log"
"$SERVER" --bind 127.0.0.1 --port "$PORT" --mode strict >"$log" 2>&1 &
pid=$!

cleanup() {
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}
trap cleanup EXIT

python3 - <<PY
import socket
import time

deadline = time.time() + 10
while True:
    try:
        with socket.create_connection(("127.0.0.1", int("${PORT}")), timeout=0.2):
            break
    except OSError:
        if time.time() > deadline:
            raise
        time.sleep(0.05)
PY

"$BENCH" \
    --host 127.0.0.1 \
    --port "$PORT" \
    --clients 50 \
    --requests "$REQUESTS" \
    --pipeline 16 \
    --workload set \
    --keyspace 10000 \
    --datasize "$DATASIZE" \
    --json-out "$JSON_OUT" \
    --key-prefix "$KEY_PREFIX" \
    >/dev/null
