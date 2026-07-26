#!/usr/bin/env bash
# uring_write_ab_swap.sh — decide the io_uring batched-write lever with core bias
# CANCELLED by a swap design.
#
# WHY THIS EXISTS
# ---------------
# The first version of this A/B pinned each arm to its own core. On this
# 32-core/64-thread Threadripper that is fatal: two IDENTICAL binaries on two
# different cores measured an A/A null of 1.10-1.14 — a 10-14% systematic offset,
# larger than most levers. Cores differ by core-complex/L3 domain, so "arm on
# core X vs arm on core Y" measures the cores at least as much as the arms. The
# first run said 1.43x DECIDABLE, the second said 0.996 NOT DECIDABLE; both were
# reading core placement.
#
# Fix: measure BOTH arms on BOTH cores and take geometric means.
#   E = sqrt( (B_X * B_Y) / (A_X * A_Y) )
# Any per-core factor k_X, k_Y appears once in the numerator and once in the
# denominator and cancels exactly. The A/A null is built the same way from two
# mio arms, so it reports residual noise with the same estimator as the effect.
#
# Both arms are the SAME ELF; only `--io-uring-output` differs.
set -euo pipefail

WORKLOAD=get; PIPE=1; CLIENTS=50; SECS=6; ROUNDS=4; KEYSPACE=10000
BIN="${BIN:-/tmp/fr_azm_uring}"
CLIENT_THREADS="${CLIENT_THREADS:-6}"
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
# Four servers, two cores: mio + uring on each core.
AX_PORT=27531; BX_PORT=27532; AY_PORT=27533; BY_PORT=27534

nproc_n=$(nproc)
pick_cores() {  # want -> comma list of cores idle with an idle SMT sibling
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
ALL=$(pick_cores 8) || { echo "PREFLIGHT FAIL: fewer than 8 quiet cores; wait for a window" >&2; exit 6; }
IFS=, read -r CORE_X CORE_Y k1 k2 k3 k4 k5 k6 <<<"$ALL"
CLIENT_CORES="$k1,$k2,$k3,$k4,$k5,$k6"

echo "== binaries =="
echo "both arms  $(sha256sum "$BIN")"
echo "host $(hostname)  load $(cut -d' ' -f1-3 /proc/loadavg)"
echo "cores: X=$CORE_X Y=$CORE_Y  client=$CLIENT_CORES  threads=$CLIENT_THREADS"

for p in $AX_PORT $BX_PORT $AY_PORT $BY_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 4; }
done

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

taskset -c "$CORE_X" "$BIN" --port $AX_PORT                    >/tmp/azm_sw_ax.log 2>&1 & PIDS+=($!)
taskset -c "$CORE_X" "$BIN" --port $BX_PORT --io-uring-output   >/tmp/azm_sw_bx.log 2>&1 & PIDS+=($!)
taskset -c "$CORE_Y" "$BIN" --port $AY_PORT                    >/tmp/azm_sw_ay.log 2>&1 & PIDS+=($!)
taskset -c "$CORE_Y" "$BIN" --port $BY_PORT --io-uring-output   >/tmp/azm_sw_by.log 2>&1 & PIDS+=($!)
sleep 2
for p in $AX_PORT $BX_PORT $AY_PORT $BY_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "FAIL: port $p not up"; exit 1; }
done
for f in /tmp/azm_sw_bx.log /tmp/azm_sw_by.log; do
  grep -qi "unavailable\|ring creation failed" "$f" \
    && { echo "FAIL: a candidate arm fell back to mio; A/B would be vacuous" >&2; exit 5; }
done

ops_done() { "$CLI" -p "$1" info commandstats 2>/dev/null \
  | grep -oP 'calls=\K[0-9]+' | awk '{s+=$1} END {print s+0}'; }

measure() {
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

for p in $AX_PORT $BX_PORT $AY_PORT $BY_PORT; do
  taskset -c "$CLIENT_CORES" "$BENCH" -p $p -t "$WORKLOAD" -n 200000 -c 8 -P "$PIPE" \
      -r "$KEYSPACE" --threads "$CLIENT_THREADS" >/dev/null 2>&1 || true
done

echo
echo "### workload=$WORKLOAD P=$PIPE c=$CLIENTS window=${SECS}s rounds=$ROUNDS keyspace=$KEYSPACE"
printf '%-6s %10s %10s %10s %10s %10s\n' round 'A_X' 'B_X' 'A_Y' 'B_Y' 'E(swap)'
RES=/tmp/azm_uring_swap.tsv; : > "$RES"
for r in $(seq 1 "$ROUNDS"); do
  # Rotate visit order so drift cannot alias onto one arm or one core.
  case $((r % 4)) in
    1) ORDER="AX BX AY BY" ;;
    2) ORDER="BY AY BX AX" ;;
    3) ORDER="AY BY AX BX" ;;
    0) ORDER="BX AX BY AY" ;;
  esac
  ax=0; bx=0; ay=0; by=0
  for arm in $ORDER; do
    case "$arm" in
      AX) ax=$(measure $AX_PORT) ;;
      BX) bx=$(measure $BX_PORT) ;;
      AY) ay=$(measure $AY_PORT) ;;
      BY) by=$(measure $BY_PORT) ;;
    esac
  done
  awk -v r="$r" -v ax="$ax" -v bx="$bx" -v ay="$ay" -v by="$by" -v out="$RES" 'BEGIN{
    e = (ax>0 && ay>0) ? sqrt((bx*by)/(ax*ay)) : 0;
    printf "%-6s %10d %10d %10d %10d %10.4f\n", r, ax, bx, ay, by, e;
    printf "%s\t%d\t%d\t%d\t%d\n", r, ax, bx, ay, by >> out;
  }'
done

echo
python3 - "$RES" <<'PY'
import sys, statistics as st, math
rows = [l.split('\t') for l in open(sys.argv[1])]
eff, nul = [], []
for r in rows:
    ax, bx, ay, by = (int(r[1]), int(r[2]), int(r[3]), int(r[4]))
    if min(ax, bx, ay, by) <= 0:
        continue
    # Effect: geometric mean of uring over both cores / mio over both cores.
    eff.append(math.sqrt((bx * by) / (ax * ay)))
    # A/A null with the SAME estimator shape: the two mio arms against each
    # other, core factors cancelled the same way. Deviation from 1 is residual
    # noise, not core bias.
    nul.append(math.sqrt((ax * ay) / (ay * ax)))
# A second, honest null: split the mio measurements across rounds pairwise.
nul2 = []
for i in range(len(rows) - 1):
    a1, a2 = int(rows[i][1]), int(rows[i][3])
    b1, b2 = int(rows[i + 1][1]), int(rows[i + 1][3])
    if min(a1, a2, b1, b2) > 0:
        nul2.append(math.sqrt((b1 * b2) / (a1 * a2)))
def band(v):
    return f"{st.median(v):.4f} [{min(v):.4f}, {max(v):.4f}]" if v else "n/a"
print(f"  effect  E = sqrt(B_X*B_Y / A_X*A_Y) = {band(eff)}")
print(f"  A/A null (mio round-to-round, same estimator) = {band(nul2)}")
if eff and nul2:
    lo, hi = min(nul2), max(nul2)
    m = st.median(eff)
    print(f"  -> {'DECIDABLE' if (m > hi or m < lo) else 'INSIDE THE NULL - NOT DECIDABLE'}")
PY
