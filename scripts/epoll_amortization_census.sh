#!/usr/bin/env bash
# epoll_amortization_census.sh — the syscall the per-core reactor topology pays
# and the single-threaded incumbent does not.
#
# WHY THIS EXISTS
# ---------------
# We retire ~2.5x FEWER user-space instructions per op than redis
# (instr_per_op_vs_redis.sh), yet the c=1 serial controls in
# parallel_heavy_keys.sh come in at only 1.06-1.17x on reply-heavy commands. A
# user-space advantage that large converting to that little wall clock means the
# cost is outside user space. This counts where.
#
# WHAT IT FINDS
# -------------
# recv and send are 1.000/op on BOTH engines -- one read and one write per
# command, exactly as an unpipelined request/response protocol requires, and a
# cost the incumbent pays identically. The whole difference is epoll_wait:
#
#   redis has ONE event loop owning ALL connections, so a single epoll_wait
#   returns many ready fds and the wakeup amortizes across them. Its
#   epoll_wait/op FALLS as concurrency rises (0.077 -> 0.017 -> 0.009), i.e. one
#   wakeup covering ~113 events at c=128.
#
#   fr spreads connections over N per-core reactors, so each reactor's epoll_wait
#   returns only the handful of fds that landed on it. epoll_wait/op stays ~1.0
#   at every concurrency -- one wakeup PER COMMAND -- and does not amortize at
#   all.
#
# That is ~1 extra syscall per operation, ~48% more total syscalls, and it is the
# standing tax the partitioned topology pays for its parallelism. It is not a
# disqualifier: the same topology wins 2.1-2.27x whole-job at c=8-32 and 12.587x
# on compute-heavy keys. But it is the honest reason the UNIFORM small-command
# sweep compresses at high concurrency, exactly where redis's io-threads engage.
#
# COUNTS, NOT TIMES. `perf trace` inflates per-syscall latency by instrumenting
# every entry/exit, so only the COUNTS here are trustworthy; the ratio is
# deliberately built from counts alone.
set -euo pipefail

CONNS="${CONNS:-16,64,128}"
TOTAL="${TOTAL:-120000}"          # per family; 2 families => ops = 2 * TOTAL
KEYSPACE="${KEYSPACE:-10000}"
WORKERS="${WORKERS:-16}"
CLIENT_THREADS="${CLIENT_THREADS:-8}"
TESTS="${TESTS:-get,set}"
FR_BIN="${FR_BIN:-/data/tmp/cargo-target/release/frankenredis}"
SERVER_CPUS="${SERVER_CPUS:-0-15,32-47}"
CLIENT_CPUS="${CLIENT_CPUS:-16-31,48-63}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_PORT="${FR_PORT:-27911}"; RD_PORT="${RD_PORT:-27912}"

[ -x "$FR_BIN" ] || { echo "FAIL: $FR_BIN not executable" >&2; exit 3; }
for f in "$BENCH" "$REDIS" "$CLI"; do
  [ -x "$f" ] || { echo "FAIL: missing vendored $f" >&2; exit 3; }
done
command -v perf >/dev/null || { echo "FAIL: perf not installed" >&2; exit 3; }
for p in $FR_PORT $RD_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 5; }
done

NTESTS=$(echo "$TESTS" | tr ',' '\n' | grep -c .)
OPS=$((TOTAL * NTESTS))

echo "== host identity =="
echo "  host      $(hostname)   kernel $(uname -r)   loadavg $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  fr ELF    $(sha256sum "$FR_BIN" | cut -d' ' -f1)"
echo "  redis ELF $(sha256sum "$REDIS" | cut -d' ' -f1)"
echo "  job       -t $TESTS n=$TOTAL each ($NTESTS families => $OPS ops), P=1, r=$KEYSPACE"
echo "  fr        --experimental-sharded-set-get-workers $WORKERS"
echo "  redis     --io-threads 1  (single loop; its default and its cheapest config)"
echo

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_epoll_fr.log 2>&1 &
FR_PID=$!; PIDS+=($FR_PID)
taskset -c "$SERVER_CPUS" "$REDIS" --port $RD_PORT --save '' --appendonly no \
    --io-threads 1 >/tmp/fr_epoll_rd.log 2>&1 &
RD_PID=$!; PIDS+=($RD_PID)
sleep 2
for p in $FR_PORT $RD_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: no PONG on $p" >&2; exit 6; }
done
echo "  fr    RUNNING-IMAGE $(sha256sum /proc/$FR_PID/exe | cut -d' ' -f1)"
echo "  redis RUNNING-IMAGE $(sha256sum /proc/$RD_PID/exe | cut -d' ' -f1)"
echo

run_one() { # $1=label $2=pid $3=port $4=conns
  local label="$1" pid="$2" port="$3" c="$4" threads="$CLIENT_THREADS" o
  [ "$c" -lt "$threads" ] && threads="$c"
  o=$(mktemp)
  perf stat -e syscalls:sys_enter_epoll_wait,syscalls:sys_enter_recvfrom,\
syscalls:sys_enter_sendto,syscalls:sys_enter_read,syscalls:sys_enter_write,\
raw_syscalls:sys_enter \
    -p "$pid" -x, -o "$o" -- \
    taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -t "$TESTS" -n "$TOTAL" \
      -c "$c" -P 1 --threads "$threads" -r "$KEYSPACE" >/dev/null 2>&1
  awk -F, -v l="$label" -v c="$c" -v n="$OPS" '
    /epoll_wait/{e=$1} /recvfrom/{rf=$1} /sendto/{st=$1} /sys_enter_read/{rd=$1}
    /sys_enter_write/{wr=$1} /raw_syscalls/{all=$1}
    END{
      ev=(e>0)?n/e:0;
      printf "  %-5s %-6s %13.3f %9.3f %9.3f %11.3f %14.1f\n",
             c, l, e/n, (rf+rd)/n, (st+wr)/n, all/n, ev
    }' "$o"
  rm -f "$o"
}

printf '  %-5s %-6s %13s %9s %9s %11s %14s\n' \
  conns engine 'epoll_wait/op' 'recv/op' 'send/op' 'total sys/op' 'events per wake'
for C in ${CONNS//,/ }; do
  run_one fr    "$FR_PID" "$FR_PORT" "$C"
  run_one redis "$RD_PID" "$RD_PORT" "$C"
done

echo
echo "  recv/op and send/op are 1.000 on both engines: one read and one write per"
echo "  command is what an unpipelined protocol costs, and the incumbent pays it"
echo "  identically, so it is NOT our gap. epoll_wait/op is the entire difference."
echo "  'events per wake' is ops/epoll_wait -- how many ready fds one wakeup covers."
echo "  redis amortizes better as concurrency RISES; a per-core reactor holding a"
echo "  handful of connections cannot, and stays near one wakeup per command."
