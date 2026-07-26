#!/usr/bin/env bash
# broad_headtohead.sh — fr vs vendored redis across redis-benchmark's FULL
# default suite, so a loss on any command surfaces instead of being missed by a
# three-workload spot check.
#
# The 2026-07-26 re-measurement found fr ahead on SET/GET/INCR/TTL-SET at both
# depths. That is four commands. This runs every test redis-benchmark ships
# (PING, SET, GET, INCR, LPUSH/RPUSH, LPOP/RPOP, SADD, HSET, SPOP, ZADD,
# ZPOPMIN, LRANGE at four sizes, MSET) against both engines, alternating which
# engine goes first each round, and reports the per-test ratio.
#
# Statistic is the median of per-round ratios. Cores are preflighted for
# contention and for idle SMT siblings, because core identity alone biases
# throughput by 10-14% on this part.
set -euo pipefail

PIPE=16; CLIENTS=50; REQUESTS=100000; ROUNDS=3; KEYSPACE=10000
FR_BIN="${FR_BIN:-/tmp/fr_azm_head}"
while [ $# -gt 0 ]; do
  case "$1" in
    -P) PIPE="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -n) REQUESTS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_PORT=27551; RD_PORT=27552

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
IFS=, read -r FR_CORE RD_CORE CLIENT_CORE <<<"$ALL"

echo "== binaries =="
echo "fr    $(sha256sum "$FR_BIN")"
echo "redis $(sha256sum "$REDIS")"
echo "host $(hostname) load $(cut -d' ' -f1-3 /proc/loadavg)"
echo "cores: fr=$FR_CORE redis=$RD_CORE client=$CLIENT_CORE  P=$PIPE c=$CLIENTS n=$REQUESTS rounds=$ROUNDS"

for p in $FR_PORT $RD_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 4; }
done

FR_PID=""; RD_PID=""
cleanup() { for p in $FR_PID $RD_PID; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT
taskset -c "$FR_CORE" "$FR_BIN" --port $FR_PORT >/tmp/azm_bh_fr.log 2>&1 & FR_PID=$!
taskset -c "$RD_CORE" "$REDIS" --port $RD_PORT --save '' --appendonly no >/tmp/azm_bh_rd.log 2>&1 & RD_PID=$!
sleep 2
for p in $FR_PORT $RD_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "FAIL: port $p not up"; exit 1; }
done

RES=/tmp/azm_broad.tsv; : > "$RES"
run_suite() {  # PORT TAG ROUND
  taskset -c "$CLIENT_CORE" "$BENCH" -p "$1" -n "$REQUESTS" -c "$CLIENTS" -P "$PIPE" \
      -r "$KEYSPACE" --csv 2>/dev/null \
    | tail -n +2 \
    | awk -F, -v tag="$2" -v r="$3" '{gsub(/"/,""); print r"\t"tag"\t"$1"\t"$2}' >> "$RES"
}
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then
    run_suite $FR_PORT fr "$r"; run_suite $RD_PORT redis "$r"
  else
    run_suite $RD_PORT redis "$r"; run_suite $FR_PORT fr "$r"
  fi
  echo "  round $r done"
done

echo
python3 - "$RES" <<'PY'
import sys, statistics as st
from collections import defaultdict
d = defaultdict(lambda: defaultdict(dict))
for line in open(sys.argv[1]):
    parts = line.rstrip("\n").split("\t")
    if len(parts) != 4:
        continue
    r, tag, test, rps = parts
    try:
        d[test][r][tag] = float(rps)
    except ValueError:
        continue
rows = []
for test, rounds in d.items():
    ratios = [v["fr"] / v["redis"] for v in rounds.values()
              if "fr" in v and "redis" in v and v["redis"] > 0]
    if not ratios:
        continue
    frs = [v["fr"] for v in rounds.values() if "fr" in v]
    rds = [v["redis"] for v in rounds.values() if "redis" in v]
    rows.append((st.median(ratios), test, st.median(frs), st.median(rds), min(ratios), max(ratios)))
rows.sort()
print(f"{'test':<28}{'fr ops/s':>12}{'redis ops/s':>13}{'fr/redis':>10}   [min,max]")
for ratio, test, fr, rd, lo, hi in rows:
    flag = "  <-- fr SLOWER" if ratio < 0.97 else ""
    print(f"{test:<28}{fr:>12,.0f}{rd:>13,.0f}{ratio:>10.3f}   [{lo:.3f},{hi:.3f}]{flag}")
losses = [r for r in rows if r[0] < 0.97]
print(f"\n{len(losses)} of {len(rows)} tests show fr below 0.97x")
PY
