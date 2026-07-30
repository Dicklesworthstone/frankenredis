#!/usr/bin/env bash
# thread_scaling_headtohead.sh — the structural claim that SCALES.
#
# WHAT THIS MEASURES
# ------------------
# redis-server is single-threaded BY DESIGN on command execution. Every
# per-operation win this repository has banked (io_uring output batching, the XADD
# shape family) was measured against a rival using one execution thread, so those
# ratios do not compound with load. This harness asks the question that does:
# under a whole job over many concurrent pipelined clients, how does the
# FrankenRedis/Redis ratio move as we add execution threads?
#
# The SHAPE is the finding. A ratio that widens with our worker count is a
# structural win. A ratio that flattens means something else saturated first, and
# the flattening point names it.
#
# SCOPE LIMIT -- READ THIS BEFORE QUOTING ANY NUMBER
# --------------------------------------------------
# FrankenRedis's only multi-threaded execution path is
# `--experimental-sharded-set-get-workers N`, whose own help text reads: "Run
# exact default-DB SET/GET on N key shards (1-256); permits local PING/QUIT and
# refuses every other command". It is additionally incompatible with hardened
# mode, --config, --aof, --rdb, --replicaof and --enable-debug-command.
#
# So a MIXED command workload is not measurable on this path -- every command
# outside SET/GET/PING/QUIT is refused. This harness therefore runs a SET/GET job,
# and every row it prints is scoped to SET/GET. The capability limit is part of
# the result, not a footnote: the scaling headroom is real but currently reachable
# only in a mode that serves two commands.
#
# FAIRNESS
# --------
# Nobody should be able to call this rigged, so:
#   * All three server arms get the SAME cpuset (24 physical cores, both SMT
#     siblings each). Arms are measured one at a time, never concurrently, so an
#     identical cpuset is honest and it removes the 10-14% core-IDENTITY bias that
#     invalidated three earlier readings in this repository.
#   * redis-server gets its best honest configuration: its own documented scaling
#     knob `io-threads` (with io-threads-do-reads), persistence off. It cannot use
#     more cores for command execution -- that is the structural fact under test,
#     not a handicap we imposed.
#   * The A/A null is a second FrankenRedis at the SAME worker count, measured in
#     the same invocation, so the null tracks the effect's own scale.
#   * ACTUAL OBSERVED thread count is read from /proc/<pid>/status, never assumed
#     from the requested worker count.
# Gate is a bootstrap 95% median CI with a 2x null margin. CV is never computed.
set -euo pipefail

WORKERS="1,8,32,64,128"
ROUNDS=3
TOTAL=200000          # per redis-benchmark test; the JOB is SET then GET
CLIENTS=128
PIPE=16
KEYSPACE=100000
REDIS_IO_THREADS=8
FR_BIN="${FR_BIN:-/data/tmp/cargo-target/release/frankenredis}"
SERVER_CPUS="${SERVER_CPUS:-0-23,32-55}"
CLIENT_CPUS="${CLIENT_CPUS:-24-31,56-63}"

while [ $# -gt 0 ]; do
  case "$1" in
    -W) WORKERS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -n) TOTAL="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
    --io-threads) REDIS_IO_THREADS="$2"; shift 2;;
    --bin) FR_BIN="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_PORT=27811; RD_PORT=27812; FR2_PORT=27813

[ -x "$FR_BIN" ] || { echo "FAIL: fr binary not executable: $FR_BIN" >&2; exit 3; }
for f in "$BENCH" "$REDIS" "$CLI"; do
  [ -x "$f" ] || { echo "FAIL: missing vendored $f" >&2; exit 3; }
done

# --- host identity, recorded on every row -----------------------------------
HOST="$(hostname)"
KERNEL="$(uname -r)"
CPU_MODEL="$(awk -F': ' '/model name/{print $2; exit}' /proc/cpuinfo)"
NPROC="$(nproc)"
FR_SHA="$(sha256sum "$FR_BIN" | cut -d' ' -f1)"
RD_SHA="$(sha256sum "$REDIS" | cut -d' ' -f1)"

echo "== host identity =="
echo "  host          $HOST"
echo "  kernel        $KERNEL"
echo "  cpu           $CPU_MODEL ($NPROC threads)"
echo "  loadavg       $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  fr    ELF     $FR_SHA"
echo "  redis ELF     $RD_SHA"
echo "  server cpuset $SERVER_CPUS   client cpuset $CLIENT_CPUS"
echo "  job           SET then GET, n=$TOTAL each, c=$CLIENTS, P=$PIPE, r=$KEYSPACE"
echo "  redis config  --io-threads $REDIS_IO_THREADS --io-threads-do-reads yes --save '' --appendonly no"
echo "  SCOPE         SET/GET only (sharded path refuses every other command)"

# fr must actually support the flag; an older binary silently predates it. A
# sweep of only W=0 needs no flag, which is how a release-perf binary predating
# the sharded path can still serve as the build-profile control arm.
if [ "$(echo "$WORKERS" | tr -d '0,')" != "" ] \
   && ! "$FR_BIN" --help 2>&1 | grep -q -- "--experimental-sharded-set-get-workers"; then
  echo "FAIL: $FR_BIN does not support --experimental-sharded-set-get-workers" >&2
  exit 4
fi

for p in $FR_PORT $RD_PORT $FR2_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 5; }
done

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

# redis-server stays up for the whole sweep: its configuration does not vary with
# our worker count, and restarting it would discard its keyspace mid-comparison.
taskset -c "$SERVER_CPUS" "$REDIS" --port $RD_PORT --save '' --appendonly no \
    --io-threads "$REDIS_IO_THREADS" --io-threads-do-reads yes \
    >/tmp/azm_ts_redis.log 2>&1 &
RD_PID=$!; PIDS+=($RD_PID)
sleep 2
"$CLI" -p $RD_PORT ping >/dev/null 2>&1 || { echo "FAIL: redis not up"; tail -5 /tmp/azm_ts_redis.log; exit 6; }
RD_THREADS=$(awk '/^Threads:/{print $2}' /proc/$RD_PID/status 2>/dev/null)
echo "  redis observed threads at rest: $RD_THREADS"
echo "  redis RUNNING-IMAGE sha256:     $(running_image_sha $RD_PID)"
echo "  builder identity:               ${FR_BUILDER:-unrecorded — set FR_BUILDER}"
echo

observed_threads() { awk '/^Threads:/{print $2}' /proc/"$1"/status 2>/dev/null || echo 0; }

# Provenance for the process that actually produced the numbers. Hashing the path
# we launched proves only that a file with that name had that content; hashing
# /proc/<pid>/exe hashes the image the KERNEL MAPPED for the running server, which
# is what a reader needs when binaries are staged, rebuilt and swapped in /tmp by
# several agents on one host. Recorded alongside where the binary was built,
# because a binary of unknown origin is not evidence.
running_image_sha() { sha256sum /proc/"$1"/exe 2>/dev/null | cut -d' ' -f1; }

# Whole-job wall time: the JOB is the full SET test followed by the full GET test.
# Returns aggregate ops/s over the whole job.
run_job() {
  local port="$1" t0 t1
  t0=$(date +%s.%N)
  taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -t set,get -n "$TOTAL" \
      -c "$CLIENTS" -P "$PIPE" -r "$KEYSPACE" --threads 8 >/dev/null 2>&1
  t1=$(date +%s.%N)
  awk -v s="$t0" -v e="$t1" -v n="$TOTAL" 'BEGIN{d=e-s; printf "%.0f", (d>0)?(2*n)/d:0}'
}

RES=/tmp/azm_thread_scaling.tsv; : > "$RES"
printf '%-8s %-6s %12s %12s %12s %9s %8s\n' workers round 'fr ops/s' 'redis ops/s' 'fr2 ops/s' 'fr/redis' 'obs_thr'

for W in ${WORKERS//,/ }; do
  # W=0 is the REFERENCE arm: normal single-threaded FrankenRedis with no sharded
  # flag at all. Without it the curve cannot say whether multi-threading helps or
  # hurts relative to our own shipped path, only how it compares to Redis.
  if [ "$W" = 0 ]; then
    SHARD_ARGS=()
  else
    SHARD_ARGS=(--experimental-sharded-set-get-workers "$W")
  fi
  # fr arms restart per worker count -- the count is fixed at startup.
  taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR_PORT \
      "${SHARD_ARGS[@]}" >/tmp/azm_ts_fr.log 2>&1 &
  FR_PID=$!; PIDS+=($FR_PID)
  taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR2_PORT \
      "${SHARD_ARGS[@]}" >/tmp/azm_ts_fr2.log 2>&1 &
  FR2_PID=$!; PIDS+=($FR2_PID)
  sleep 2
  ok=1
  for p in $FR_PORT $FR2_PORT; do
    "$CLI" -p "$p" ping >/dev/null 2>&1 || ok=0
  done
  if [ "$ok" -ne 1 ]; then
    echo "SKIP workers=$W: fr arm did not come up"; tail -3 /tmp/azm_ts_fr.log
    kill -9 $FR_PID $FR2_PID 2>/dev/null || true
    continue
  fi
  echo "  W=$W fr RUNNING-IMAGE sha256: $(running_image_sha $FR_PID)  (null arm: $(running_image_sha $FR2_PID))"

  for r in $(seq 1 "$ROUNDS"); do
    # Alternate arm order so a warming or drifting host cannot favour one engine.
    if [ $((r % 2)) -eq 1 ]; then
      a=$(run_job $FR_PORT);  b=$(run_job $RD_PORT); n2=$(run_job $FR2_PORT)
    else
      b=$(run_job $RD_PORT);  a=$(run_job $FR_PORT); n2=$(run_job $FR2_PORT)
    fi
    # Threads observed AFTER load: worker threads may be created lazily.
    thr=$(observed_threads $FR_PID)
    ratio=$(awk -v x="$a" -v y="$b" 'BEGIN{printf "%.4f", (y>0)?x/y:0}')
    printf '%-8s %-6s %12d %12d %12d %9s %8s\n' "$W" "$r" "$a" "$b" "$n2" "$ratio" "$thr"
    printf '%s\t%s\t%s\t%s\t%s\n' "$W" "$a" "$b" "$n2" "$thr" >> "$RES"
  done
  kill -9 $FR_PID $FR2_PID 2>/dev/null || true
  sleep 1
done

python3 "$ROOT/scripts/_thread_scaling_stats.py" "$RES"
echo
echo "host=$HOST kernel=$KERNEL cpu=$NPROC-thread fr_elf=${FR_SHA:0:16} redis_elf=${RD_SHA:0:16}"
echo "SCOPE: SET/GET only. The sharded execution path refuses every other command."
