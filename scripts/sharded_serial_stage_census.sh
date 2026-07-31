#!/usr/bin/env bash
# sharded_serial_stage_census.sh — WHY does the sharded SET/GET path lose?
#
# THE QUESTION
# ------------
# redis-server executes commands on ONE thread. A sharded design that scales
# should therefore dominate it. Measured three times, `--experimental-sharded-
# set-get-workers` instead LOSES 2.3-3.0x and its curve peaks at W=1. A loss that
# large means our sharding pays a cost redis never pays. This harness names that
# cost by COUNTING it per thread, rather than asserting it from a flamegraph.
#
# WHAT IT MEASURES, AND WHY PER-THREAD
# ------------------------------------
# The sharded path did not remove FrankenRedis's single-threaded stage: the mio
# event loop still reads every socket, parses every frame, routes it, restores
# per-connection reply order and writes every reply. The workers execute only the
# store hit. So the design is Amdahl's law with a named serial stage, and the
# decisive quantity is how many instructions that ONE thread executes per
# operation, sharded versus not:
#
#   ceiling(W -> infinity) = (event-loop instructions/op at W=0)
#                          / (event-loop instructions/op at W=N)
#
# If that ratio is below 1.0 the serial stage got MORE expensive, and no worker
# count can win -- the path is unfixable by construction rather than untuned.
#
# WHY COUNTS AND NOT WALL CLOCK
# -----------------------------
# instructions:u per thread, divided by an EXACT denominator (redis-benchmark
# -t set,get -n N issues exactly 2N operations), is a ratio of counts. It does
# not move with host load, client speed or core identity -- the three effects
# that produced this repository's three known false positives. A starved client
# makes the server idle longer; it does not change instructions per operation.
# Wall clock and CPU% are recorded for provenance and are gated on nothing.
#
# THREAD GROUPS
#   evloop   the mio event loop (process main thread) -- the SERIAL stage
#   shard    fr-set-get-shard-N worker threads        -- the PARALLEL stage
#   other    writer pool and any runtime helper thread
set -euo pipefail

WORKERS="0,1,8,32"
ROUNDS=3
TOTAL=1000000          # per redis-benchmark test; the JOB is SET then GET, ops=2*TOTAL
CLIENTS=128
PIPE=16
KEYSPACE=100000
CLIENT_THREADS=6
FR_BIN="${FR_BIN:-/tmp/fr_census_956a5ab34}"
SERVER_CPUS="${SERVER_CPUS:-0-7,32-39}"
CLIENT_CPUS="${CLIENT_CPUS:-8-15,40-47}"
FR_PORT="${FR_PORT:-27831}"
OUTDIR="${OUTDIR:-/tmp/sharded_census}"

while [ $# -gt 0 ]; do
  case "$1" in
    -W) WORKERS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -n) TOTAL="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
    --bin) FR_BIN="$2"; shift 2;;
    --port) FR_PORT="$2"; shift 2;;
    --out) OUTDIR="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"

[ -x "$FR_BIN" ] || { echo "FAIL: fr binary not executable: $FR_BIN" >&2; exit 3; }
for f in "$BENCH" "$CLI"; do
  [ -x "$f" ] || { echo "FAIL: missing vendored $f" >&2; exit 3; }
done
command -v perf >/dev/null || { echo "FAIL: perf not found" >&2; exit 3; }
"$FR_BIN" --help 2>&1 | grep -q -- "--experimental-sharded-set-get-workers" \
  || { echo "FAIL: $FR_BIN predates the sharded flag" >&2; exit 4; }
ss -ltn 2>/dev/null | grep -q ":$FR_PORT " && { echo "PREFLIGHT FAIL: port $FR_PORT bound" >&2; exit 5; }

# Tracepoints are needed for the syscall columns. Prove access up front rather
# than silently reporting zeros for the syscalls this whole question is about.
EVENTS="instructions:u,task-clock,context-switches"
if perf stat -e 'syscalls:sys_enter_futex' -x, true >/dev/null 2>&1; then
  EVENTS="$EVENTS,syscalls:sys_enter_futex,syscalls:sys_enter_write"
  TRACEPOINTS=yes
else
  TRACEPOINTS=no
fi

mkdir -p "$OUTDIR"
HOST="$(hostname)"
FR_SHA="$(sha256sum "$FR_BIN" | cut -d' ' -f1)"
running_image_sha() { sha256sum /proc/"$1"/exe 2>/dev/null | cut -d' ' -f1; }

echo "== sharded serial-stage census =="
echo "  host          $HOST  kernel $(uname -r)  loadavg $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  cpu           $(awk -F': ' '/model name/{print $2; exit}' /proc/cpuinfo)"
echo "  fr ELF        $FR_SHA"
echo "  fr path       $FR_BIN"
echo "  server cpuset $SERVER_CPUS   client cpuset $CLIENT_CPUS"
echo "  job           SET then GET, n=$TOTAL each => ops=$((2*TOTAL)) EXACT, c=$CLIENTS, P=$PIPE, r=$KEYSPACE"
echo "  events        $EVENTS  (tracepoints=$TRACEPOINTS)"
echo "  rounds        $ROUNDS per worker count"
echo

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

RES="$OUTDIR/census.tsv"; : > "$RES"

for W in ${WORKERS//,/ }; do
  if [ "$W" = 0 ]; then SHARD_ARGS=(); else SHARD_ARGS=(--experimental-sharded-set-get-workers "$W"); fi

  taskset -c "$SERVER_CPUS" "$FR_BIN" --port "$FR_PORT" "${SHARD_ARGS[@]}" \
      >"$OUTDIR/fr_W$W.log" 2>&1 &
  FR_PID=$!; PIDS+=("$FR_PID")
  sleep 2
  if ! "$CLI" -p "$FR_PORT" ping >/dev/null 2>&1; then
    echo "SKIP W=$W: arm did not come up"; tail -3 "$OUTDIR/fr_W$W.log"
    kill -9 "$FR_PID" 2>/dev/null || true; continue
  fi
  # Provenance belongs to the PROCESS that produced the numbers, not to a path.
  echo "W=$W  running-image sha256 $(running_image_sha "$FR_PID")  threads-at-rest $(awk '/^Threads:/{print $2}' /proc/"$FR_PID"/status)"

  for r in $(seq 1 "$ROUNDS"); do
    PERF_OUT="$OUTDIR/perf_W${W}_r${r}.csv"
    t0=$(date +%s.%N)
    # perf follows the SERVER pid for exactly the duration of the client job, so
    # the measured window and the exact 2N denominator describe the same interval.
    perf stat --per-thread -e "$EVENTS" -x, -o "$PERF_OUT" -p "$FR_PID" -- \
      taskset -c "$CLIENT_CPUS" "$BENCH" -p "$FR_PORT" -t set,get -n "$TOTAL" \
        -c "$CLIENTS" -P "$PIPE" -r "$KEYSPACE" --threads "$CLIENT_THREADS" \
        >"$OUTDIR/bench_W${W}_r${r}.log" 2>&1
    t1=$(date +%s.%N)
    elapsed=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.3f", b-a}')
    # FR_PID is recorded so the stats pass can identify the event-loop thread
    # EXACTLY (tid == pid), instead of guessing it from a truncated comm string.
    printf '%s\t%s\t%s\t%s\t%s\n' "$W" "$r" "$elapsed" "$PERF_OUT" "$FR_PID" >> "$RES"
    echo "  W=$W round $r  elapsed ${elapsed}s  ops/s $(awk -v n="$((2*TOTAL))" -v d="$elapsed" 'BEGIN{printf "%.0f", (d>0)?n/d:0}')"
  done

  kill -9 "$FR_PID" 2>/dev/null || true
  sleep 1
done

echo
python3 "$ROOT/scripts/_sharded_serial_stage_stats.py" "$RES" "$((2*TOTAL))"
echo
echo "host=$HOST fr_elf=${FR_SHA:0:16}"
echo "SCOPE: SET/GET only. The sharded execution path refuses every other command."
