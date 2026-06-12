#!/usr/bin/env bash
set -euo pipefail

REMOTE="${REMOTE:-vmi1152480}"
REMOTE_REPO="${REMOTE_REPO:-/data/projects/.scratch/frankenredis-coralox-pass147-20260612T224346Z}"
REMOTE_BIN="${REMOTE_BIN:-./.rch-target-vmi1152480-job-29884606035525977-1781304514946242203-0/release-perf/frankenredis}"
PREFILL_N="${PREFILL_N:-1500000}"
SPOP_N="${SPOP_N:-1000000}"
CLIENTS="${CLIENTS:-50}"
PIPELINE="${PIPELINE:-16}"
KEYSPACE="${KEYSPACE:-20000000}"

ssh "$REMOTE" "cd '$REMOTE_REPO' && REMOTE_BIN='$REMOTE_BIN' PREFILL_N='$PREFILL_N' SPOP_N='$SPOP_N' CLIENTS='$CLIENTS' PIPELINE='$PIPELINE' KEYSPACE='$KEYSPACE' bash -s" <<'REMOTE_SCRIPT'
set -euo pipefail

BIN="${REMOTE_BIN:?missing REMOTE_BIN}"
BENCH="./legacy_redis_code/redis/src/redis-benchmark"
PORT=$(python3 - <<'PY'
import socket

with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
FRLOG="/tmp/coralox-qkf0z-fr-$PORT.log"
PREFILL_LOG="/tmp/coralox-qkf0z-prefill-$PORT.log"
SPOP_LOG="/tmp/coralox-qkf0z-spop-$PORT.log"

"$BIN" --port "$PORT" >"$FRLOG" 2>&1 &
FRPID=$!
cleanup() {
  kill "$FRPID" 2>/dev/null || true
  wait "$FRPID" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 80); do
  if "$BENCH" -p "$PORT" -n 1 -c 1 PING >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done

"$BENCH" -p "$PORT" -t sadd -n "${PREFILL_N:?missing PREFILL_N}" -c "${CLIENTS:?missing CLIENTS}" -P "${PIPELINE:?missing PIPELINE}" -r "${KEYSPACE:?missing KEYSPACE}" -q >"$PREFILL_LOG" 2>&1
"$BENCH" -p "$PORT" -t spop -n "${SPOP_N:?missing SPOP_N}" -c "$CLIENTS" -P "$PIPELINE" -r "$KEYSPACE" -q >"$SPOP_LOG" 2>&1
cat "$SPOP_LOG"
REMOTE_SCRIPT
