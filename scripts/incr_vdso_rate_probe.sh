#!/usr/bin/env bash
# vdso_rate.sh RKEY — is the INCR vdso cost a fixed RATE (per event-loop iteration
# / timer) or genuinely PER-OPERATION? Shares of total cycles cannot tell them
# apart when throughput also varies with -r, so measure the ABSOLUTE rate:
#   vdso cycles/s = (vdso share) x (total cycles/s)
#   vdso cycles/op = vdso cycles/s / ops per s
set -euo pipefail
RKEY="$1"; BIN="${BIN:-/tmp/fr_azm_fp}"
ROOT=/data/projects/frankenredis; PORT=27401
for p in $(ss -ltnp 2>/dev/null | grep ":$PORT" | grep -oP 'pid=\K[0-9]+' | sort -u); do kill -9 "$p"; done
sleep 1
taskset -c 56 "$BIN" --port $PORT >/tmp/azm_rate.log 2>&1 &
sleep 2
PID="$(ss -ltnp 2>/dev/null | grep ":$PORT" | grep -oP 'pid=\K[0-9]+' | head -1)"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
taskset -c 60 "$BENCH" -p $PORT -n 300000 -c 50 -P 16 -r "$RKEY" INCR counter:__rand_int__ >/dev/null 2>&1
taskset -c 60 "$BENCH" -p $PORT -n 100000000 -c 50 -P 16 -r "$RKEY" INCR counter:__rand_int__ >/dev/null 2>&1 &
BPID=$!
sleep 3
# (1) cycles + ops over the SAME 6 s window
O0=$("$CLI" -p $PORT info commandstats 2>/dev/null | grep -oP 'calls=\K[0-9]+' | awk '{s+=$1} END{print s+0}')
CYC=$(sudo -n perf stat -x, -e cycles -p "$PID" -- sleep 6 2>&1 | awk -F, '/cycles/{print $1}')
O1=$("$CLI" -p $PORT info commandstats 2>/dev/null | grep -oP 'calls=\K[0-9]+' | awk '{s+=$1} END{print s+0}')
OPS=$((O1 - O0))
# (2) vdso share over a separate window on the same steady state
sudo -n perf record -g -F 1999 -p "$PID" -o /tmp/azm_rate.data -- sleep 6 >/dev/null 2>&1
SHARE=$(sudo -n perf report -i /tmp/azm_rate.data --stdio --no-children --sort dso 2>/dev/null \
        | grep -E "^\s+[0-9.]+%\s+\[vdso\]" | grep -oP '^\s+\K[0-9.]+')
kill -9 "$BPID" 2>/dev/null || true; wait "$BPID" 2>/dev/null || true
kill -9 "$PID" 2>/dev/null || true
awk -v r="$RKEY" -v c="$CYC" -v o="$OPS" -v s="${SHARE:-0}" 'BEGIN{
  cps=c/6; ops=o/6; vc=cps*s/100;
  printf "-r %-7s  ops/s=%9.0f  cycles/s=%12.0f  vdso=%5.2f%%  vdso_cycles/s=%11.0f  vdso_cycles/op=%7.1f\n",
         r, ops, cps, s, vc, (ops>0? vc/ops : 0);
}'
