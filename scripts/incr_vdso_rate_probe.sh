#!/usr/bin/env bash
# incr_vdso_rate_probe.sh RKEY — is the INCR vdso cost a fixed RATE (per event-loop iteration
# / timer) or genuinely PER-OPERATION? Shares of total cycles cannot tell them
# apart when throughput also varies with -r, so measure the ABSOLUTE rate:
#   vdso cycles/s = (vdso share) x (total cycles/s)
#   vdso cycles/op = vdso cycles/s / ops per s
set -euo pipefail
export LC_ALL=C
if [ "$#" -ne 1 ]; then
  echo "usage: $0 <keyspace-size>" >&2
  exit 2
fi

RKEY="$1"
BIN="${BIN:-/tmp/fr_azm_fp}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-27401}"
SERVER_CORE="${SERVER_CORE:-56}"
CLIENT_CORE="${CLIENT_CORE:-60}"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
STAMP="$$.$(date +%s)"
LOG="/tmp/fr_incr_vdso_rate_${STAMP}.log"
PERF_DATA="/tmp/fr_incr_vdso_rate_${STAMP}.data"

if ! [[ "$RKEY" =~ ^[1-9][0-9]*$ ]]; then
  echo "PREFLIGHT FAIL: keyspace size must be a positive integer: $RKEY" >&2
  exit 3
fi
if ! [[ "$PORT" =~ ^[0-9]+$ ]] || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
  echo "PREFLIGHT FAIL: port must be an integer in 1..65535: $PORT" >&2
  exit 3
fi
for core in "$SERVER_CORE" "$CLIENT_CORE"; do
  if ! [[ "$core" =~ ^[0-9]+$ ]] || [ ! -d "/sys/devices/system/cpu/cpu$core" ]; then
    echo "PREFLIGHT FAIL: unavailable CPU core: $core" >&2
    exit 3
  fi
done
if [ "$SERVER_CORE" -eq "$CLIENT_CORE" ]; then
  echo "PREFLIGHT FAIL: server and client must use different CPU cores" >&2
  exit 3
fi

for executable in "$BIN" "$CLI" "$BENCH"; do
  if [ ! -x "$executable" ]; then
    echo "PREFLIGHT FAIL: executable not found: $executable" >&2
    exit 3
  fi
done
if ss -ltn 2>/dev/null | grep -q ":$PORT "; then
  echo "PREFLIGHT FAIL: port $PORT is already bound; refusing to kill or reuse its owner" >&2
  exit 4
fi

check_core() {
  local core="$1" role="$2" busy
  busy=$(ps -eo psr,pcpu,comm --no-headers \
    | awk -v c="$core" '$1==c && $2>10 && $3!~/^(frankenredis|redis-benchmark|perf)$/ {printf "%s(%.0f%%) ", $3, $2}')
  if [ -n "$busy" ]; then
    echo "PREFLIGHT FAIL: core $core ($role) is contended by: $busy" >&2
    exit 5
  fi
}
check_core "$SERVER_CORE" "server"
check_core "$CLIENT_CORE" "client"

PID=""
BPID=""
cleanup() {
  [ -n "$BPID" ] && kill "$BPID" 2>/dev/null || true
  [ -n "$BPID" ] && wait "$BPID" 2>/dev/null || true
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  [ -n "$PID" ] && wait "$PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "bench_elf_sha256=$(sha256sum "$BIN" | awk '{print $1}') bytes=$(wc -c < "$BIN") path=$BIN"
taskset -c "$SERVER_CORE" "$BIN" --port "$PORT" >"$LOG" 2>&1 &
PID=$!
sleep 2
if ! kill -0 "$PID" 2>/dev/null || ! "$CLI" -p "$PORT" ping >/dev/null 2>&1; then
  echo "PREFLIGHT FAIL: server did not become ready; log=$LOG" >&2
  exit 6
fi

taskset -c "$CLIENT_CORE" "$BENCH" -p "$PORT" -n 300000 -c 50 -P 16 -r "$RKEY" INCR counter:__rand_int__ >/dev/null 2>&1
taskset -c "$CLIENT_CORE" "$BENCH" -p "$PORT" -n 100000000 -c 50 -P 16 -r "$RKEY" INCR counter:__rand_int__ >/dev/null 2>&1 &
BPID=$!
sleep 3
# (1) cycles + ops over the SAME 6 s window
O0=$("$CLI" -p "$PORT" info commandstats 2>/dev/null | awk -F'[=,]' '/^cmdstat_incr:/{print $2+0}')
CYC=$(sudo -n perf stat -x, -e cycles -p "$PID" -- sleep 6 2>&1 | awk -F, '/cycles/{print $1}')
O1=$("$CLI" -p "$PORT" info commandstats 2>/dev/null | awk -F'[=,]' '/^cmdstat_incr:/{print $2+0}')
if ! [[ "$O0" =~ ^[0-9]+$ && "$O1" =~ ^[0-9]+$ && "$CYC" =~ ^[0-9]+$ ]]; then
  echo "MEASUREMENT INVALID: incr_before=${O0:-missing} incr_after=${O1:-missing} cycles=${CYC:-missing}" >&2
  exit 7
fi
OPS=$((O1 - O0))
if [ "$OPS" -le 0 ]; then
  echo "MEASUREMENT INVALID: cycles=${CYC:-missing} incr_ops=$OPS" >&2
  exit 7
fi
# (2) vdso share over a separate window on the same steady state
sudo -n perf record -g -F 1999 -p "$PID" -o "$PERF_DATA" -- sleep 6 >/dev/null 2>&1
SHARE=$(sudo -n perf report -i "$PERF_DATA" --stdio --no-children --sort dso 2>/dev/null \
        | grep -E "^\s+[0-9.]+%\s+\[vdso\]" | grep -oP '^\s+\K[0-9.]+')
awk -v r="$RKEY" -v c="$CYC" -v o="$OPS" -v s="${SHARE:-0}" 'BEGIN{
  cps=c/6; ops=o/6; vc=cps*s/100;
  printf "-r %-7s  ops/s=%9.0f  cycles/s=%12.0f  vdso=%5.2f%%  vdso_cycles/s=%11.0f  vdso_cycles/op=%7.1f\n",
         r, ops, cps, s, vc, (ops>0? vc/ops : 0);
}'
