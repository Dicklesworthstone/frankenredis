#!/usr/bin/env bash
# profile_hot_path.sh — symbol-resolved CPU profile of the frankenredis server
# hot path under a redis-benchmark workload, reporting fr-code self-time so a
# perf lever can be picked with evidence (extreme-software-optimization step 1).
#
# WHY THIS EXISTS: `rch` offloads cargo and retrieves the *standard* `--release`
# artifact, but the release profile sets strip=true so the retrieved binary has
# NO symbols, and the release-perf profile's linked binary is NOT retrieved at
# all (only metadata). Profiling has therefore been blocked across many sessions.
# The fix below builds the RELEASE profile (so rch retrieves it) but overrides
# strip=false + debug=1 via --config, yielding a ~41MB *symbol-bearing* binary
# that rch DOES retrieve to $CARGO_TARGET_DIR/release/frankenredis.
#
# Usage:
#   scripts/profile_hot_path.sh [-t set|get|incr|lpush|...] [-P pipeline]
#                               [-n requests] [-c clients] [-s seconds]
#                               [-r keyspace] [--no-build]
# Example: scripts/profile_hot_path.sh -t set -P 16 -s 8
set -euo pipefail

BENCH_T=set; PIPE=16; N=5000000; CLIENTS=50; SECS=8; KEYSPACE=100000; BUILD=1
while [ $# -gt 0 ]; do
  case "$1" in
    -t) BENCH_T="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -n) N="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -s) SECS="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
    --no-build) BUILD=0; shift;;
    *) echo "unknown arg $1"; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TGT="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
BIN="$TGT/release/frankenredis"
REDIS_BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
PORT=23939
PERF_DATA="/data/tmp/claude-1000/profile_hot_path_$$.data"

if [ "$BUILD" = 1 ]; then
  echo ">> building symbol-bearing release binary via rch ..."
  rch exec -- cargo build --release -p fr-server \
      --config 'profile.release.strip=false' --config 'profile.release.debug=1' >/dev/null 2>&1
fi
[ -x "$BIN" ] || { echo "FAIL: $BIN not found/retrieved"; exit 1; }
file "$BIN" | grep -q "not stripped" || echo "WARN: binary appears stripped (symbols may be missing)"

pkill -9 -f "frankenredis --port $PORT" 2>/dev/null || true
sleep 1
"$BIN" --port $PORT >/tmp/profile_fr.log 2>&1 &
sleep 2
FRPID="$(ss -ltnp 2>/dev/null | grep ":$PORT" | grep -oP 'pid=\K[0-9]+' | head -1)"
[ -n "$FRPID" ] || { echo "FAIL: fr did not start"; cat /tmp/profile_fr.log; exit 1; }
echo ">> fr pid=$FRPID, load: -t $BENCH_T -P $PIPE -c $CLIENTS -n $N -r $KEYSPACE"

"$REDIS_BENCH" -p $PORT -t "$BENCH_T" -n "$N" -c "$CLIENTS" -P "$PIPE" -r "$KEYSPACE" \
    >/tmp/profile_bench.log 2>&1 &
BPID=$!
sleep 1
rm -f "$PERF_DATA"
perf record -g -F 999 -p "$FRPID" -o "$PERF_DATA" -- sleep "$SECS" 2>/dev/null || true
kill -9 "$BPID" 2>/dev/null || true

echo "== throughput =="
grep -iE "requests per second" /tmp/profile_bench.log | tail -1 || true
echo "== fr-code self-time hotspots (DSO=frankenredis) =="
perf report -i "$PERF_DATA" --stdio --dsos="$(basename "$BIN")" --percent-limit 0.8 2>/dev/null \
    | grep -E "^[[:space:]]+[0-9]+\." | head -30
kill -9 "$FRPID" 2>/dev/null || true
echo ">> perf data: $PERF_DATA"
