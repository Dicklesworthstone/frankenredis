#!/usr/bin/env bash
# uring_write_ab.sh — decide the io_uring batched-write lever against its own
# A/A null, per the campaign bench-harness contract.
#
# Three servers, one core each, all from the SAME source tree:
#   armA  = control, mio write path            (binary $CTL_BIN)
#   armB  = candidate, --io-uring-output       (binary $URING_BIN)
#   armN  = second control                     -> armA/armN is the A/A NULL
#
# Arm order alternates every round; the statistic is the MEDIAN of per-round
# ratios. A claim is decidable only if it sits outside the A/A null band.
#
# Targets the UNPIPELINED regime deliberately: the 2026-07-25 syscall census
# measured 1.00 submission syscalls/op at -P1 (47.4% of wall) but only 0.13-0.16
# at -P16, so there is nothing to batch at depth. Pass -P to check that claim.
set -euo pipefail

WORKLOAD=get; PIPE=1; CLIENTS=50; SECS=6; ROUNDS=5; KEYSPACE=10000
# ONE-BINARY A/B: both arms are the SAME ELF. Only `--io-uring-output` differs,
# so codegen is identical and the measured delta is purely the runtime write
# path. (A separate feature-off build would also differ by the compiled-out
# `uring_writes_active()` check, confounding the comparison.)
CTL_BIN="${CTL_BIN:-/tmp/fr_azm_uring}"
URING_BIN="${URING_BIN:-/tmp/fr_azm_uring}"
# AUTO CORE SELECTION. This host runs ~22 agents; hand-picked cores get claimed
# between one run and the next, and a contended arm produces a clean but WRONG
# ratio. Pick cores that are idle AND whose SMT sibling is idle (siblings are
# N and N+32 on this 32-core/64-thread part), so neither arm shares an execution
# unit with someone else's build.
pick_cores() {
  local want="$1" out=() n
  n=$(nproc)
  for c in $(seq 0 $((n - 1))); do
    local sib=$(( c >= n/2 ? c - n/2 : c + n/2 ))
    local u v
    u=$(ps -eo psr,pcpu --no-headers | awk -v c="$c" '$1==c {t+=$2} END {printf "%.0f", t+0}')
    v=$(ps -eo psr,pcpu --no-headers | awk -v c="$sib" '$1==c {t+=$2} END {printf "%.0f", t+0}')
    if [ "$u" -lt 2 ] && [ "$v" -lt 2 ]; then
      out+=("$c")
      [ "${#out[@]}" -ge "$want" ] && break
    fi
  done
  [ "${#out[@]}" -lt "$want" ] && { echo "" ; return 1; }
  ( IFS=,; echo "${out[*]}" )
}
if [ -z "${A_CORE:-}" ]; then
  ALL=$(pick_cores 9) || { echo "PREFLIGHT FAIL: fewer than 9 quiet cores available; wait for a window" >&2; exit 6; }
  IFS=, read -r A_CORE B_CORE N_CORE c1 c2 c3 c4 c5 c6 <<<"$ALL"
  CLIENT_CORES="${CLIENT_CORES:-$c1,$c2,$c3,$c4,$c5,$c6}"
  echo "auto-selected cores: armA=$A_CORE armB=$B_CORE armN=$N_CORE client=$CLIENT_CORES"
fi
A_CORE="${A_CORE:-61}"; B_CORE="${B_CORE:-62}"; N_CORE="${N_CORE:-63}"
CLIENT_CORE="${CLIENT_CORE:-60}"
CLIENT_THREADS="${CLIENT_THREADS:-4}"
CLIENT_CORES="${CLIENT_CORES:-$CLIENT_CORE}"
while [ $# -gt 0 ]; do
  case "$1" in
    -t) WORKLOAD="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -s) SECS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
A_PORT=27511; B_PORT=27512; N_PORT=27513

echo "== binaries =="
echo "armA/armN control  $(sha256sum "$CTL_BIN")"
echo "armB   candidate   $(sha256sum "$URING_BIN")"
echo "host $(hostname)  load $(cut -d' ' -f1-3 /proc/loadavg)"

# Contended cores produce a clean, reproducible WRONG answer; refuse to start.
for spec in "$A_CORE:armA" "$B_CORE:armB" "$N_CORE:armN" "$CLIENT_CORE:client"; do
  core="${spec%%:*}"; role="${spec##*:}"
  busy=$(ps -eo psr,pcpu,comm --no-headers \
    | awk -v c="$core" '$1==c && $2>10 && $3!~/^(frankenredis|redis-server|redis-benchmark|perf)$/ {printf "%s(%.0f%%) ", $3, $2}')
  [ -n "$busy" ] && { echo "PREFLIGHT FAIL: core $core ($role) contended by: $busy" >&2; exit 3; }
done
for p in $A_PORT $B_PORT $N_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p already bound" >&2; exit 4; }
done

A_PID=""; B_PID=""; N_PID=""
cleanup() { for p in $A_PID $B_PID $N_PID; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

taskset -c "$A_CORE" "$CTL_BIN"   --port $A_PORT                      >/tmp/azm_ab_a.log 2>&1 & A_PID=$!
taskset -c "$B_CORE" "$URING_BIN" --port $B_PORT --io-uring-output    >/tmp/azm_ab_b.log 2>&1 & B_PID=$!
taskset -c "$N_CORE" "$CTL_BIN"   --port $N_PORT                      >/tmp/azm_ab_n.log 2>&1 & N_PID=$!
sleep 2
for p in $A_PORT $B_PORT $N_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "FAIL: port $p not up"; exit 1; }
done
# The candidate must actually be running the ring, not silently on the mio path.
grep -qi "io_uring batched writes unavailable\|ring creation failed" /tmp/azm_ab_b.log \
  && { echo "FAIL: candidate fell back to mio; the A/B would be vacuous" >&2; exit 5; }
echo "servers: armA=$A_PID(core$A_CORE) armB=$B_PID(core$B_CORE) armN=$N_PID(core$N_CORE)"

ops_done() { "$CLI" -p "$1" info commandstats 2>/dev/null \
  | grep -oP 'calls=\K[0-9]+' | awk '{s+=$1} END {print s+0}'; }

measure() { # PORT -> ops in window
  local port="$1"
  taskset -c "$CLIENT_CORES" "$BENCH" -p "$port" -t "$WORKLOAD" -n 100000000 \
      -c "$CLIENTS" -P "$PIPE" -r "$KEYSPACE" --threads "$CLIENT_THREADS" >/dev/null 2>&1 &
  local bpid=$!
  sleep 1
  local o0; o0=$(ops_done "$port")
  sleep "$SECS"
  local o1; o1=$(ops_done "$port")
  kill -9 $bpid 2>/dev/null || true; wait $bpid 2>/dev/null || true
  echo $(( (o1 - o0) / SECS ))
}

for p in $A_PORT $B_PORT $N_PORT; do
  taskset -c "$CLIENT_CORES" "$BENCH" -p $p -t "$WORKLOAD" -n 200000 -c 8 -P "$PIPE" \
      -r "$KEYSPACE" --threads "$CLIENT_THREADS" >/dev/null 2>&1 || true
done

echo
echo "### workload=$WORKLOAD P=$PIPE c=$CLIENTS window=${SECS}s rounds=$ROUNDS keyspace=$KEYSPACE"
printf '%-6s %12s %12s %12s %10s %10s\n' round 'armA ops/s' 'armB ops/s' 'armN ops/s' 'B/A' 'N/A(null)'
RES=/tmp/azm_uring_ab.tsv; : > "$RES"
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then ORDER="A B N"; else ORDER="B A N"; fi
  a=0; b=0; n=0
  for arm in $ORDER; do
    case "$arm" in
      A) a=$(measure $A_PORT) ;;
      B) b=$(measure $B_PORT) ;;
      N) n=$(measure $N_PORT) ;;
    esac
  done
  awk -v r="$r" -v a="$a" -v b="$b" -v n="$n" -v out="$RES" 'BEGIN{
    printf "%-6s %12d %12d %12d %10.4f %10.4f\n", r, a, b, n, (a?b/a:0), (a?n/a:0);
    printf "%s\t%d\t%d\t%d\n", r, a, b, n >> out;
  }'
done

echo
python3 - "$RES" <<'PY'
import sys, statistics as st
rows = [line.split('\t') for line in open(sys.argv[1])]
ba = sorted(int(r[2])/int(r[1]) for r in rows if int(r[1]))
na = sorted(int(r[3])/int(r[1]) for r in rows if int(r[1]))
def band(v): return f"{st.median(v):.4f} [{min(v):.4f}, {max(v):.4f}]"
print(f"  effect  armB/armA = {band(ba)}")
print(f"  A/A null armN/armA = {band(na)}")
lo, hi = min(na), max(na)
m = st.median(ba)
verdict = "DECIDABLE" if (m > hi or m < lo) else "INSIDE THE NULL - NOT DECIDABLE"
print(f"  -> {verdict}")
PY
