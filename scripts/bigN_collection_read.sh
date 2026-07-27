#!/usr/bin/env bash
# bigN_collection_read.sh — hunt Class-1 structural weakness: big-N collection
# reads, where the incumbent pays a per-element cost.
#
# WHY THIS SHAPE
# --------------
# The full-suite head-to-head shows FrankenRedis's advantage on LRANGE growing
# monotonically with N:  100 -> 1.164x,  300 -> 1.307x,  500 -> 1.312x,
# 600 -> 1.444x.  redis-benchmark's built-in LRANGE stops at 600, so nobody has
# measured where that curve goes. A ratio that rises with N is the signature of a
# per-element cost the incumbent pays and we do not — the same shape that gives
# frankenpandas 19.5x on 1M-row GroupBy.
#
# This prefills one collection of N elements on BOTH engines and reads it whole,
# sweeping N. Both servers run side-by-side in the same invocation (Policy 2), on
# verified-idle cores whose SMT siblings are also idle, arm order alternating,
# statistic = median of per-round ratios, with a second fr process as the A/A
# null arm.
set -euo pipefail

TYPE=list; SIZES="100,1000,10000,100000"; CLIENTS=8; PIPE=1; ROUNDS=3; REQS=2000
FR_BIN="${FR_BIN:-/tmp/fr_azm_head}"
while [ $# -gt 0 ]; do
  case "$1" in
    -T) TYPE="$2"; shift 2;;
    -N) SIZES="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -n) REQS="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_PORT=27581; RD_PORT=27582; FR2_PORT=27583

nproc_n=$(nproc)
pick_cores() {
  local want="$1" out=() c sib u v
  for c in $(seq 0 $((nproc_n - 1))); do
    sib=$(( c >= nproc_n/2 ? c - nproc_n/2 : c + nproc_n/2 ))
    u=$(ps -eo psr,pcpu --no-headers | awk -v k="$c"   '$1==k {t+=$2} END {printf "%.0f", t+0}')
    v=$(ps -eo psr,pcpu --no-headers | awk -v k="$sib" '$1==k {t+=$2} END {printf "%.0f", t+0}')
    if [ "$u" -lt 2 ] && [ "$v" -lt 2 ]; then
      out+=("$c"); [ "${#out[@]}" -ge "$want" ] && break
    fi
  done
  [ "${#out[@]}" -lt "$want" ] && return 1
  ( IFS=,; echo "${out[*]}" )
}
ALL=$(pick_cores 4) || { echo "PREFLIGHT FAIL: fewer than 4 quiet cores; wait for a window" >&2; exit 6; }
IFS=, read -r FR_CORE RD_CORE FR2_CORE CLIENT_CORE <<<"$ALL"

echo "== binaries =="
echo "fr    $(sha256sum "$FR_BIN")"
echo "redis $(sha256sum "$REDIS")"
echo "host $(hostname) load $(cut -d' ' -f1-3 /proc/loadavg)"
echo "cores fr=$FR_CORE redis=$RD_CORE frA/A=$FR2_CORE client=$CLIENT_CORE  type=$TYPE P=$PIPE c=$CLIENTS"

for p in $FR_PORT $RD_PORT $FR2_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 4; }
done
PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT
taskset -c "$FR_CORE"  "$FR_BIN" --port $FR_PORT  >/tmp/azm_bn_fr.log  2>&1 & PIDS+=($!)
taskset -c "$FR2_CORE" "$FR_BIN" --port $FR2_PORT >/tmp/azm_bn_fr2.log 2>&1 & PIDS+=($!)
taskset -c "$RD_CORE"  "$REDIS"  --port $RD_PORT --save '' --appendonly no >/tmp/azm_bn_rd.log 2>&1 & PIDS+=($!)
sleep 2
for p in $FR_PORT $RD_PORT $FR2_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "FAIL: port $p not up"; exit 1; }
done

# Prefill an identical collection of N elements on every arm.
prefill() {
  local port="$1" n="$2"
  "$CLI" -p "$port" del bigcoll >/dev/null 2>&1 || true
  case "$TYPE" in
    list) seq 1 "$n" | awk '{print "RPUSH bigcoll e"$1}' | "$CLI" -p "$port" --pipe >/dev/null 2>&1 ;;
    hash) seq 1 "$n" | awk '{print "HSET bigcoll f"$1" v"$1}' | "$CLI" -p "$port" --pipe >/dev/null 2>&1 ;;
    zset) seq 1 "$n" | awk '{print "ZADD bigcoll "$1" m"$1}'  | "$CLI" -p "$port" --pipe >/dev/null 2>&1 ;;
    set)  seq 1 "$n" | awk '{print "SADD bigcoll m"$1}'       | "$CLI" -p "$port" --pipe >/dev/null 2>&1 ;;
  esac
}
read_cmd() {
  case "$TYPE" in
    list) echo "LRANGE bigcoll 0 -1" ;;
    hash) echo "HGETALL bigcoll" ;;
    zset) echo "ZRANGE bigcoll 0 -1" ;;
    set)  echo "SMEMBERS bigcoll" ;;
  esac
}

ops_done() { "$CLI" -p "$1" info commandstats 2>/dev/null \
  | grep -oP 'calls=\K[0-9]+' | awk '{s+=$1} END {print s+0}'; }

measure() {  # PORT -> ops/s
  local port="$1" o0 o1 t0 t1
  o0=$(ops_done "$port"); t0=$(date +%s.%N)
  # shellcheck disable=SC2086
  taskset -c "$CLIENT_CORE" "$BENCH" -p "$port" -n "$REQS" -c "$CLIENTS" -P "$PIPE" \
      $(read_cmd) >/dev/null 2>&1
  t1=$(date +%s.%N); o1=$(ops_done "$port")
  awk -v a="$o0" -v b="$o1" -v s="$t0" -v e="$t1" 'BEGIN{d=e-s; printf "%.0f", (d>0)?(b-a)/d:0}'
}

echo
printf '%-9s %12s %12s %12s %9s %9s\n' N 'fr ops/s' 'redis ops/s' 'frA/A ops/s' 'fr/redis' 'A/A null'
RES=/tmp/azm_bigN.tsv; : > "$RES"
for N in ${SIZES//,/ }; do
  for p in $FR_PORT $RD_PORT $FR2_PORT; do prefill "$p" "$N"; done
  # verify all three arms hold an identically sized collection
  szf=$("$CLI" -p $FR_PORT eval "return 1" 0 >/dev/null 2>&1; \
        case "$TYPE" in list) "$CLI" -p $FR_PORT llen bigcoll;; hash) "$CLI" -p $FR_PORT hlen bigcoll;; \
        zset) "$CLI" -p $FR_PORT zcard bigcoll;; set) "$CLI" -p $FR_PORT scard bigcoll;; esac)
  szr=$(case "$TYPE" in list) "$CLI" -p $RD_PORT llen bigcoll;; hash) "$CLI" -p $RD_PORT hlen bigcoll;; \
        zset) "$CLI" -p $RD_PORT zcard bigcoll;; set) "$CLI" -p $RD_PORT scard bigcoll;; esac)
  if [ "$szf" != "$szr" ] || [ "$szf" != "$N" ]; then
    echo "ABORT N=$N: arms hold different collection sizes (fr=$szf redis=$szr want=$N)" >&2
    exit 7
  fi
  for r in $(seq 1 "$ROUNDS"); do
    if [ $((r % 2)) -eq 1 ]; then a=$(measure $FR_PORT); b=$(measure $RD_PORT)
    else                          b=$(measure $RD_PORT); a=$(measure $FR_PORT); fi
    n2=$(measure $FR2_PORT)
    echo -e "$N\t$r\t$a\t$b\t$n2" >> "$RES"
  done
  python3 "$ROOT/scripts/_bigN_row.py" "$RES" "$N"
done

echo
echo "A rising fr/redis with N is the Class-1 signature: a per-element cost the"
echo "incumbent pays and we do not. A flat ratio means the shape is not a weakness."
