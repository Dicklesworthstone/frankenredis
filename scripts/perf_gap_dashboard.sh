#!/usr/bin/env bash
# perf_gap_dashboard.sh — measure the vs-redis pipelined throughput gap per
# command (the NO-GAPS "measure every gap" step). Launches a fresh config-less
# frankenredis and a fresh config-less vendored redis on neighbouring ports,
# runs redis-benchmark -P16 best-of-N against each, and prints req/s + the
# redis/fr ratio (>1.0 = fr faster, <1.0 = fr slower).
#
# Honest-measurement notes:
#   - Both servers are config-less (compiled defaults) so the comparison is fair.
#   - The ratio is measured back-to-back on the SAME host, so shared contention
#     cancels in the ratio even when absolute numbers sag under load. For
#     publishable ABSOLUTE Score>=2.0 numbers, re-run on a quiet host with the
#     SHIPPED release binary (not a debug-symboled one) and taskset-pinning.
#   - Default builds the standard --release binary via rch (stripped, fastest).
#     Pass --bin PATH to benchmark a specific binary.
#
# Usage: scripts/perf_gap_dashboard.sh [--bin PATH] [--redis-bin PATH]
#                                      [-n requests] [-P pipeline] [-c clients]
#                                      [-r keyspace] [--reps N] [--cmds "set get ..."]
set -uo pipefail

N=400000; PIPE=16; CLIENTS=50; KEYSPACE=100000; REPS=3; BUILD=1
CMDS="set get incr lpush rpush sadd hset zadd spop"
BIN=""; REDIS_BIN=""
while [ $# -gt 0 ]; do
  case "$1" in
    --bin) BIN="$2"; shift 2;;
    --redis-bin) REDIS_BIN="$2"; shift 2;;
    -n) N="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
    --reps) REPS="$2"; shift 2;;
    --cmds) CMDS="$2"; shift 2;;
    --no-build) BUILD=0; shift;;
    *) echo "unknown arg $1"; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TGT="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
[ -n "$BIN" ] || BIN="$TGT/release/frankenredis"
[ -n "$REDIS_BIN" ] || REDIS_BIN="$ROOT/legacy_redis_code/redis/src/redis-server"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
FPORT=23960; RPORT=23961

if [ "$BUILD" = 1 ] && [ -z "${BIN##$TGT/*}" ]; then
  echo ">> building standard --release frankenredis via rch ..."
  rch exec -- cargo build --release -p fr-server >/dev/null 2>&1 || true
fi
[ -x "$BIN" ] || { echo "FAIL: fr binary $BIN not found (pass --bin PATH)"; exit 1; }
[ -x "$REDIS_BIN" ] || { echo "FAIL: redis-server $REDIS_BIN not found"; exit 1; }

pkill -9 -f "frankenredis --port $FPORT" 2>/dev/null || true
pkill -9 -f "redis-server --port $RPORT" 2>/dev/null || true
sleep 1
nohup "$BIN" --port $FPORT >/tmp/perfgap_fr.log 2>&1 & disown
nohup "$REDIS_BIN" --port $RPORT --save '' --appendonly no >/tmp/perfgap_redis.log 2>&1 & disown
sleep 3
for pn in "fr:$FPORT" "redis:$RPORT"; do
  port="${pn##*:}"
  ss -ltn 2>/dev/null | grep -q ":$port" || { echo "FAIL: ${pn%%:*} did not start on $port"; exit 1; }
done

bench_qps() { # port cmd -> best-of-REPS req/s
  local port="$1" cmd="$2" best=0 q
  for _ in $(seq 1 "$REPS"); do
    q=$("$BENCH" -p "$port" -t "$cmd" -n "$N" -c "$CLIENTS" -P "$PIPE" -r "$KEYSPACE" -q 2>/dev/null \
        | grep -oE '[0-9]+\.[0-9]+ requests' | grep -oE '^[0-9]+' | head -1)
    [ -n "$q" ] && [ "$q" -gt "$best" ] && best="$q"
  done
  echo "$best"
}

echo "== vs-redis pipelined throughput (P$PIPE, -c$CLIENTS, -n$N, best-of-$REPS) =="
printf "%-7s %10s %10s %10s\n" "cmd" "redis" "fr" "redis/fr"
for cmd in $CMDS; do
  r=$(bench_qps $RPORT "$cmd"); f=$(bench_qps $FPORT "$cmd")
  python3 -c "r=$r;f=$f;print('%-7s %10d %10d %9.2fx  %s'%('$cmd',r,f,(r/f if f else 0),'fr-faster' if f>=r else 'FR-SLOWER'))" 2>/dev/null \
    || printf "%-7s %10s %10s\n" "$cmd" "$r" "$f"
done
pkill -9 -f "frankenredis --port $FPORT" 2>/dev/null || true
pkill -9 -f "redis-server --port $RPORT" 2>/dev/null || true
