#!/usr/bin/env bash
# head_of_line.sh — the thing a single-threaded incumbent CANNOT do.
#
# redis-server executes every command on ONE thread. `io-threads` parallelizes
# socket I/O only, never execution. So one O(N) command stalls EVERY other
# client for its whole duration. A partitioned keyspace confines that stall to
# the partition owning the heavy key.
#
# MEASURES light-client GET throughput and p99, first with the server otherwise
# idle, then with a few connections hammering an O(N) BITCOUNT over a large
# string. BITCOUNT is O(N) in COMPUTE but returns one integer, so this isolates
# EXECUTION blocking from reply-size / network effects.
#
# The headline is each engine's DEGRADATION AGAINST ITS OWN BASELINE, which makes
# the comparison independent of the two engines' different absolute speeds.
set -euo pipefail
ROOT=/data/projects/frankenredis
SP=/data/tmp/claude-1000/-data-projects-frankenredis/258fb22f-5d58-4d03-a462-8546093e21cb/scratchpad
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR="$ROOT/target/release/frankenredis"
SERVER_CPUS="0-15,32-47"; CLIENT_CPUS="16-31,48-63"
WORKERS=16
N="${N:-150000}"; CONNS="${CONNS:-32}"; ROUNDS="${ROUNDS:-3}"
HEAVY_CONNS="${HEAVY_CONNS:-4}"
BIGBYTES="${BIGBYTES:-8000000}"
FR_PORT=28411; RD_PORT=28412
HEAVY_PID=""

cleanup() {
  [ -n "$HEAVY_PID" ] && kill -9 "$HEAVY_PID" 2>/dev/null || true
  for v in ${SRV_PIDS:-}; do kill -9 "$v" 2>/dev/null || true; done
}
trap cleanup EXIT
SRV_PIDS=""

measure() { # $1=port -> "rps p99ms"
  taskset -c "$CLIENT_CPUS" "$BENCH" -p "$1" -t get -n "$N" -c "$CONNS" -P 1 \
      --threads 8 -r 100000 --csv 2>/dev/null \
    | awk -F, 'NR==2{gsub(/"/,"",$2); gsub(/"/,"",$7); printf "%s %s", $2, $7}'
}

start_heavy() { # $1=port ; sets HEAVY_PID. No command substitution: that would
                # block forever waiting on the child's stdout to close.
  python3 "$SP/heavy_loader.py" "$1" "$HEAVY_CONNS" hol:big >/dev/null 2>&1 &
  HEAVY_PID=$!
}
stop_heavy() { [ -n "$HEAVY_PID" ] && kill -9 "$HEAVY_PID" 2>/dev/null || true; HEAVY_PID=""; sleep 1; }

run_engine() { # $1=label $2=port
  local label="$1" port="$2" pid
  if [ "$label" = fr ]; then
    taskset -c "$SERVER_CPUS" "$FR" --port "$port" \
        --experimental-sharded-set-get-workers "$WORKERS" >/tmp/hol_$label.log 2>&1 &
  else
    taskset -c "$SERVER_CPUS" "$REDIS" --port "$port" --save '' --appendonly no \
        --io-threads 8 --io-threads-do-reads yes >/tmp/hol_$label.log 2>&1 &
  fi
  pid=$!; SRV_PIDS="$SRV_PIDS $pid"; sleep 2
  "$CLI" -p "$port" SETRANGE hol:big $((BIGBYTES-1)) x >/dev/null 2>&1
  local len; len=$("$CLI" -p "$port" STRLEN hol:big 2>/dev/null)
  echo "  ($label heavy key STRLEN=$len)"
  for r in $(seq 1 "$ROUNDS"); do
    read -r base_rps base_p99 <<<"$(measure "$port")"
    start_heavy "$port"; sleep 1
    read -r load_rps load_p99 <<<"$(measure "$port")"
    stop_heavy
    awk -v l="$label" -v r="$r" -v a="$base_rps" -v b="$load_rps" \
        -v c="$base_p99" -v d="$load_p99" 'BEGIN{
      printf "%-6s %-4s %11s %11s %9.2fx %10s %10s %9.1fx\n",
        l, r, a, b, (b>0)?a/b:0, c, d, (c>0)?d/c:0 }'
  done
  kill -9 "$pid" 2>/dev/null || true
}

echo "== head-of-line blocking: light GET load vs O(N) BITCOUNT over ${BIGBYTES} bytes =="
echo "   fr $(sha256sum "$FR" | cut -c1-16)  GET n=$N c=$CONNS  heavy_conns=$HEAVY_CONNS  reactors=$WORKERS"
echo
printf '%-6s %-4s %11s %11s %10s %10s %10s %10s\n' engine rnd 'GET base' 'GET+heavy' 'rps drop' 'p99 base' 'p99 heavy' 'p99 blowup'
run_engine fr "$FR_PORT"
run_engine redis "$RD_PORT"
echo
echo "  'rps drop' and 'p99 blowup' are each engine against ITS OWN baseline."
