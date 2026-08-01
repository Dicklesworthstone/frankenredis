#!/usr/bin/env bash
# instr_per_op_vs_redis.sh — the load-robust "does the incumbent pay this too?"
#
# WHY COUNTS, NOT WALL CLOCK
# --------------------------
# This box carries other tenants. At loadavg ~40 the A/A null on a wall-clock
# throughput harness ran 1.40x, i.e. two IDENTICAL binaries differed by 40%, so
# no throughput ratio smaller than that means anything. Retired instructions for
# a FIXED job do not care what else the host is running: the same job executes
# the same work whether or not a neighbour is compiling. That makes instr/op the
# only trustworthy cross-engine number under load.
#
# WHAT IT ANSWERS
# ---------------
# Directive (1): rank the whole job's cost and, for each entry, ask whether the
# incumbent pays it too. If redis-server retires FEWER instructions per op than
# we do, the difference is OUR overhead and it is a real lever. If it retires
# more, per-op work is not our gap and the remaining difference is scaling.
#
# HONEST LIMITS
# -------------
#   * instructions:u is USER-space only. redis and fr make different syscall
#     mixes, so this deliberately measures compute, not the kernel path. The
#     kernel side is 81-90% of a redis-benchmark profile on this host and is
#     measured separately; do not read this as end-to-end cost.
#   * IPC is not compared: under neighbour load IPC swings, instruction COUNT
#     does not. A lower instr/op is necessary, not sufficient, for a win.
#   * Both servers get the same cpuset, the same fixed job, and are measured one
#     at a time in alternating order.
set -euo pipefail

CONNS="${CONNS:-64}"
TOTAL="${TOTAL:-100000}"
PIPE="${PIPE:-1}"
KEYSPACE="${KEYSPACE:-100000}"
WORKERS="${WORKERS:-16}"
CLIENT_THREADS="${CLIENT_THREADS:-8}"
REDIS_IO_THREADS="${REDIS_IO_THREADS:-8}"
FR_BIN="${FR_BIN:-/data/tmp/cargo-target/release/frankenredis}"
SERVER_CPUS="${SERVER_CPUS:-0-15,32-47}"
CLIENT_CPUS="${CLIENT_CPUS:-16-31,48-63}"
FIXTURES="${FIXTURES:-scattered:set,get,incr hotkey:lpush,lpop,hset}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_PORT="${FR_PORT:-27871}"; RD_PORT="${RD_PORT:-27872}"; RD1_PORT="${RD1_PORT:-27873}"

[ -x "$FR_BIN" ] || { echo "FAIL: $FR_BIN not executable" >&2; exit 3; }
for f in "$BENCH" "$REDIS" "$CLI"; do
  [ -x "$f" ] || { echo "FAIL: missing vendored $f" >&2; exit 3; }
done
command -v perf >/dev/null || { echo "FAIL: perf not installed" >&2; exit 3; }
for p in $FR_PORT $RD_PORT $RD1_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 5; }
done

echo "== host identity =="
echo "  host      $(hostname)   kernel $(uname -r)   loadavg $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  fr ELF    $(sha256sum "$FR_BIN" | cut -d' ' -f1)"
echo "  redis ELF $(sha256sum "$REDIS" | cut -d' ' -f1)"
echo "  job       n=$TOTAL per family (3 families), c=$CONNS, P=$PIPE, r=$KEYSPACE"
echo "  fr        --experimental-sharded-set-get-workers $WORKERS"
echo "  redis     --io-threads $REDIS_IO_THREADS --io-threads-do-reads yes"
echo

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_ipo_fr.log 2>&1 &
FR_PID=$!; PIDS+=($FR_PID)
taskset -c "$SERVER_CPUS" "$REDIS" --port $RD_PORT --save '' --appendonly no \
    --io-threads "$REDIS_IO_THREADS" --io-threads-do-reads yes >/tmp/fr_ipo_rd.log 2>&1 &
RD_PID=$!; PIDS+=($RD_PID)
# Redis 7's IOThreadMain busy-waits (a bounded spin of up to ~1e6 iterations)
# while polling for work, so `perf stat -p` on an io-threads build bills that
# idle spinning to the job and reports absurd instr/op. Its DEFAULT single
# threaded config retires only real work, so both are measured: RD1 is the
# honest "work per op" arm, RD is what the throughput harness actually runs.
taskset -c "$SERVER_CPUS" "$REDIS" --port $RD1_PORT --save '' --appendonly no \
    --io-threads 1 >/tmp/fr_ipo_rd1.log 2>&1 &
RD1_PID=$!; PIDS+=($RD1_PID)
sleep 2
for p in $FR_PORT $RD_PORT $RD1_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: no PONG on $p" >&2; exit 6; }
done
echo "  fr    RUNNING-IMAGE $(sha256sum /proc/$FR_PID/exe | cut -d' ' -f1)"
echo "  redis RUNNING-IMAGE $(sha256sum /proc/$RD_PID/exe | cut -d' ' -f1)"
echo

# perf-stat the SERVER pid across one fixed job, then divide by the known op
# count. Attributing to the server pid keeps the client's own work out.
census() { # $1=pid $2=port $3=tests -> "instr_per_op futex_per_op"
  local pid="$1" port="$2" tests="$3" ops out futex instr
  ops=$((TOTAL * $(echo "$3" | tr ',' '\n' | grep -c .)))
  out=$(mktemp)
  perf stat -e instructions:u,syscalls:sys_enter_futex -p "$pid" -x, -o "$out" -- \
    taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -t "$tests" -n "$TOTAL" \
      -c "$CONNS" -P "$PIPE" -r "$KEYSPACE" --threads "$CLIENT_THREADS" >/dev/null 2>&1
  instr=$(awk -F, '/instructions/{print $1}' "$out")
  futex=$(awk -F, '/sys_enter_futex/{print $1}' "$out")
  rm -f "$out"
  awk -v i="${instr:-0}" -v f="${futex:-0}" -v n="$ops" 'BEGIN{printf "%.1f %.4f", i/n, f/n}'
}

printf '%-10s %-8s %14s %14s %10s %12s\n' fixture engine 'instr/op' 'futex/op' 'ratio' 'verdict'
for FX in $FIXTURES; do
  name="${FX%%:*}"; tests="${FX##*:}"
  # Warm both so first-touch allocation is not billed to the census.
  "$BENCH" -p $FR_PORT -t "$tests" -n 20000 -c "$CONNS" -P "$PIPE" -r "$KEYSPACE" >/dev/null 2>&1
  "$BENCH" -p $RD_PORT -t "$tests" -n 20000 -c "$CONNS" -P "$PIPE" -r "$KEYSPACE" >/dev/null 2>&1
  # Alternate order across the two measured passes: a COUNT should not care, and
  # if it does the host did something to one arm it did not do to the other.
  "$BENCH" -p $RD1_PORT -t "$tests" -n 20000 -c "$CONNS" -P "$PIPE" -r "$KEYSPACE" >/dev/null 2>&1
  read -r fr_i fr_f <<<"$(census "$FR_PID" "$FR_PORT" "$tests")"
  read -r rd_i rd_f <<<"$(census "$RD_PID" "$RD_PORT" "$tests")"
  read -r r1_i r1_f <<<"$(census "$RD1_PID" "$RD1_PORT" "$tests")"
  read -r r1_i2 r1_f2 <<<"$(census "$RD1_PID" "$RD1_PORT" "$tests")"
  read -r rd_i2 rd_f2 <<<"$(census "$RD_PID" "$RD_PORT" "$tests")"
  read -r fr_i2 fr_f2 <<<"$(census "$FR_PID" "$FR_PORT" "$tests")"
  awk -v n="$name" -v a="$fr_i" -v a2="$fr_i2" -v b="$rd_i" -v b2="$rd_i2" \
      -v c="$r1_i" -v c2="$r1_i2" -v af="$fr_f" -v bf="$rd_f" -v cf="$r1_f" 'BEGIN{
    fi=(a<a2)?a:a2; ri=(b<b2)?b:b2; ci=(c<c2)?c:c2;  # best-of-2: noise only adds
    printf "%-10s %-12s %14.1f %14.4f %10s %14s\n", n, "fr", fi, af, "", "";
    printf "%-10s %-12s %14.1f %14.4f %9.3fx %14s\n", "", "redis io=1", ci, cf,
      (ci>0)?fi/ci:0, (fi<ci)?"fr cheaper":"FR PAYS MORE";
    printf "%-10s %-12s %14.1f %14.4f %9.3fx %14s\n", "", "redis io=8", ri, bf,
      (ri>0)?fi/ri:0, "(spin-inflated)";
    printf "%-10s %-12s %14s %14s %10s %14s\n", "", "(spread)",
      sprintf("%.0f/%.0f", a, a2), sprintf("%.0f/%.0f", c, c2), "", "";
  }'
done
echo
echo "  'ratio' is fr instr/op divided by redis instr/op for the SAME job."
echo "  >1 means we retire more user-space instructions per operation than the"
echo "  incumbent does, which is our overhead and a lever. <1 means per-op work"
echo "  is not the gap. (spread) shows both passes per arm; a wide spread means"
echo "  the host moved under the census and the row should be re-run."
