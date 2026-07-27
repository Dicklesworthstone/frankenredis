#!/usr/bin/env bash
# bigN_swap.sh — big-N collection read, fr vs vendored redis, with core bias
# CANCELLED by a swap design.
#
# WHY THIS EXISTS
# ---------------
# scripts/bigN_collection_read.sh pins each arm to its own auto-picked core, and
# on this 32-core/64-thread part core IDENTITY alone biases throughput by 10-14%.
# It gave HGETALL@10k as 1.451x in one invocation and 1.022x in the next, with
# A/A nulls wandering 0.881-1.138. That instrument cannot decide a 1.0-1.5x
# effect, so nothing it reports should be banked.
#
# Fix, same as scripts/uring_write_ab_swap.sh: run BOTH engines on BOTH cores and
# take geometric means.
#     E = sqrt( (FR_X * FR_Y) / (RD_X * RD_Y) )
# Any per-core factor appears once in numerator and once in denominator and
# cancels exactly. The A/A null is built with the same estimator from two fr arms,
# so it reports residual noise on the same scale as the effect.
set -euo pipefail

TYPE=list; N=10000; CLIENTS=8; PIPE=1; ROUNDS=4; REQS=1500
FR_BIN="${FR_BIN:-/tmp/fr_azm_head}"
while [ $# -gt 0 ]; do
  case "$1" in
    -T) TYPE="$2"; shift 2;;
    -N) N="$2"; shift 2;;
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
FRX=27591; FRY=27592; RDX=27593; RDY=27594

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
ALL=$(pick_cores 3) || { echo "PREFLIGHT FAIL: fewer than 3 quiet cores; wait for a window" >&2; exit 6; }
IFS=, read -r CORE_X CORE_Y CLIENT_CORE <<<"$ALL"

echo "== binaries =="; echo "fr    $(sha256sum "$FR_BIN")"; echo "redis $(sha256sum "$REDIS")"
echo "host $(hostname) load $(cut -d' ' -f1-3 /proc/loadavg)"
echo "cores X=$CORE_X Y=$CORE_Y client=$CLIENT_CORE  type=$TYPE N=$N P=$PIPE c=$CLIENTS rounds=$ROUNDS"

for p in $FRX $FRY $RDX $RDY; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 4; }
done
PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT
taskset -c "$CORE_X" "$FR_BIN" --port $FRX >/tmp/azm_sw_frx.log 2>&1 & PIDS+=($!)
taskset -c "$CORE_Y" "$FR_BIN" --port $FRY >/tmp/azm_sw_fry.log 2>&1 & PIDS+=($!)
taskset -c "$CORE_X" "$REDIS" --port $RDX --save '' --appendonly no >/tmp/azm_sw_rdx.log 2>&1 & PIDS+=($!)
taskset -c "$CORE_Y" "$REDIS" --port $RDY --save '' --appendonly no >/tmp/azm_sw_rdy.log 2>&1 & PIDS+=($!)
sleep 2
for p in $FRX $FRY $RDX $RDY; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "FAIL: port $p not up"; exit 1; }
done

prefill() {
  local port="$1"
  "$CLI" -p "$port" del bigcoll >/dev/null 2>&1 || true
  case "$TYPE" in
    list) seq 1 "$N" | awk '{print "RPUSH bigcoll e"$1}'      | "$CLI" -p "$port" --pipe >/dev/null 2>&1 ;;
    hash) seq 1 "$N" | awk '{print "HSET bigcoll f"$1" v"$1}' | "$CLI" -p "$port" --pipe >/dev/null 2>&1 ;;
    zset) seq 1 "$N" | awk '{print "ZADD bigcoll "$1" m"$1}'  | "$CLI" -p "$port" --pipe >/dev/null 2>&1 ;;
    set)  seq 1 "$N" | awk '{print "SADD bigcoll m"$1}'       | "$CLI" -p "$port" --pipe >/dev/null 2>&1 ;;
  esac
}
size_of() {
  case "$TYPE" in
    list) "$CLI" -p "$1" llen bigcoll ;; hash) "$CLI" -p "$1" hlen bigcoll ;;
    zset) "$CLI" -p "$1" zcard bigcoll ;; set) "$CLI" -p "$1" scard bigcoll ;;
  esac
}
read_cmd() {
  case "$TYPE" in
    list) echo "LRANGE bigcoll 0 -1" ;; hash) echo "HGETALL bigcoll" ;;
    zset) echo "ZRANGE bigcoll 0 -1" ;; set) echo "SMEMBERS bigcoll" ;;
  esac
}
ops_done() { "$CLI" -p "$1" info commandstats 2>/dev/null \
  | grep -oP 'calls=\K[0-9]+' | awk '{s+=$1} END {print s+0}'; }
measure() {
  local port="$1" o0 o1 t0 t1
  o0=$(ops_done "$port"); t0=$(date +%s.%N)
  # shellcheck disable=SC2086
  taskset -c "$CLIENT_CORE" "$BENCH" -p "$port" -n "$REQS" -c "$CLIENTS" -P "$PIPE" \
      $(read_cmd) >/dev/null 2>&1
  t1=$(date +%s.%N); o1=$(ops_done "$port")
  awk -v a="$o0" -v b="$o1" -v s="$t0" -v e="$t1" 'BEGIN{d=e-s; printf "%.0f", (d>0)?(b-a)/d:0}'
}

for p in $FRX $FRY $RDX $RDY; do prefill "$p"; done
for p in $FRX $FRY $RDX $RDY; do
  sz=$(size_of "$p")
  [ "$sz" = "$N" ] || { echo "ABORT: port $p holds $sz elements, want $N" >&2; exit 7; }
done
echo "all four arms hold $N elements"

RES=/tmp/azm_bigN_swap.tsv; : > "$RES"
printf '\n%-6s %10s %10s %10s %10s %9s\n' round 'FR_X' 'FR_Y' 'RD_X' 'RD_Y' 'E(swap)'
for r in $(seq 1 "$ROUNDS"); do
  case $((r % 4)) in
    1) ORDER="FRX RDX FRY RDY" ;; 2) ORDER="RDY FRY RDX FRX" ;;
    3) ORDER="FRY RDY FRX RDX" ;; 0) ORDER="RDX FRX RDY FRY" ;;
  esac
  fx=0; fy=0; rx=0; ry=0
  for arm in $ORDER; do
    case "$arm" in
      FRX) fx=$(measure $FRX) ;; FRY) fy=$(measure $FRY) ;;
      RDX) rx=$(measure $RDX) ;; RDY) ry=$(measure $RDY) ;;
    esac
  done
  awk -v r="$r" -v fx="$fx" -v fy="$fy" -v rx="$rx" -v ry="$ry" -v out="$RES" 'BEGIN{
    e = (rx>0 && ry>0) ? sqrt((fx*fy)/(rx*ry)) : 0;
    printf "%-6s %10d %10d %10d %10d %9.4f\n", r, fx, fy, rx, ry, e;
    printf "%s\t%d\t%d\t%d\t%d\n", r, fx, fy, rx, ry >> out;
  }'
done

echo
python3 "$ROOT/scripts/_bigN_swap_stats.py" "$RES"
