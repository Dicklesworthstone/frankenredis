#!/usr/bin/env bash
# (frankenredis-ym6ih) Head-to-head HDEL/SREM removal throughput vs Redis 7.2.4.
# Canonical CPU-bound gate: redis-benchmark -c50 -P16 over a large pre-populated
# hashtable-range hash / set, deleting random distinct members (__rand_int__).
# Single-threaded servers pinned to a dedicated core; load-gen on cores 4-11.
set -u

FR_BIN=${FR_BIN:-/data/projects/.rch-targets/frankenredis-cc/release/frankenredis}
REDIS_BIN=${REDIS_BIN:-/data/projects/frankenredis/legacy_redis_code/redis/src/redis-server}
BENCH=${BENCH:-/data/projects/frankenredis/legacy_redis_code/redis/src/redis-benchmark}
CLI=${CLI:-/data/projects/frankenredis/legacy_redis_code/redis/src/redis-cli}
FR_PORT=17901
RD_PORT=17902
N=${N:-2000000}        # hash/set field count (>128 => hashtable encoding)
OPS=${OPS:-2000000}    # redis-benchmark request count
RUNS=${RUNS:-3}
LOADGEN="taskset -c 4-11"

cleanup() { pkill -f "port $FR_PORT" 2>/dev/null; pkill -f "port $RD_PORT" 2>/dev/null; sleep 0.3; }
trap cleanup EXIT
cleanup

# fr pinned to core 2, redis to core 3 (single-threaded; isolate per-cmd CPU)
taskset -c 2 "$FR_BIN" --port $FR_PORT >/tmp/fr_ym6ih.log 2>&1 &
taskset -c 3 "$REDIS_BIN" --port $RD_PORT --save "" --appendonly no >/tmp/rd_ym6ih.log 2>&1 &
sleep 1.2

for p in $FR_PORT $RD_PORT; do
  if ! $CLI -p $p ping >/dev/null 2>&1; then echo "server on $p down"; cat /tmp/fr_ym6ih.log; exit 1; fi
done

# ---- populate helpers (deterministic field/member names matching __rand_int__'s %012d) ----
populate_hash() { # port
  python3 - "$1" "$N" <<'PY' | $CLI -p "$1" --pipe >/dev/null
import sys
port, n = sys.argv[1], int(sys.argv[2])
out=sys.stdout
for i in range(n):
    out.write(f"HSET h field:{i:012d} v\n")
PY
}
populate_set() { # port
  python3 - "$1" "$N" <<'PY' | $CLI -p "$1" --pipe >/dev/null
import sys
port, n = sys.argv[1], int(sys.argv[2])
out=sys.stdout
for i in range(n):
    out.write(f"SADD s member:{i:012d}\n")
PY
}

rps() { grep -iEo "[0-9.]+ requests per second" | head -1 | awk '{print $1}'; }

run_cmd() { # label  populate_fn  benchargs...
  local label="$1"; local popfn="$2"; shift 2
  echo "### $label"
  for run in $(seq 1 $RUNS); do
    $CLI -p $FR_PORT flushall >/dev/null; $popfn $FR_PORT
    fr=$($LOADGEN $BENCH -p $FR_PORT -c 50 -P 16 -n $OPS -r $N "$@" 2>/dev/null | rps)
    $CLI -p $RD_PORT flushall >/dev/null; $popfn $RD_PORT
    rd=$($LOADGEN $BENCH -p $RD_PORT -c 50 -P 16 -n $OPS -r $N "$@" 2>/dev/null | rps)
    ratio=$(awk -v r="$rd" -v f="$fr" 'BEGIN{if(f>0)printf "%.3f", r/f; else print "NA"}')
    printf "  run%d  fr=%-12s redis=%-12s  redis/fr=%s\n" "$run" "$fr" "$rd" "$ratio"
  done
}

run_cmd "HDEL (hashtable hash, $N fields)" populate_hash HDEL h field:__rand_int__
run_cmd "SREM (hashtable set, $N members)" populate_set  SREM s member:__rand_int__
