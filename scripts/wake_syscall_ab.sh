#!/usr/bin/env bash
# A/A + A/B on a COUNTED metric: eventfd writes per operation in the sharded
# SET/GET handoff.
#
# Why a counted metric rather than throughput. The effect being tested is a
# syscall-rate defect: every completion batch issued an unconditional eventfd
# write, so write/op rose from 1/16 at W=1 toward 1.0 as workers scattered the
# batches. write/op is a ratio of two counts with an exact denominator (ops=2N),
# which makes it insensitive to host load -- and this host runs several agents,
# so a wall-clock A/B here would be refused by our own core-contention gate while
# a count-based one stays valid.
#
# One invocation contains all three arms: baseline, a SECOND baseline as the A/A
# null, and the candidate. Arm order rotates per round. Verdict gates on a
# bootstrap 95% median CI of per-round ratios against the null's own CI with a 2x
# margin. CV is provenance only and gates nothing.
set -euo pipefail

W=32; ROUNDS=5; N=300000
BASE_BIN="${BASE_BIN:-/tmp/fr_azm_base}"
CAND_BIN="${CAND_BIN:-/tmp/fr_azm_cand}"
SERVER_CPUS="${SERVER_CPUS:-0-23,32-55}"
CLIENT_CPUS="${CLIENT_CPUS:-24-31,56-63}"

while [ $# -gt 0 ]; do
  case "$1" in
    -W) W="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -n) N="$2"; shift 2;;
    --base) BASE_BIN="$2"; shift 2;;
    --cand) CAND_BIN="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"

for b in "$BASE_BIN" "$CAND_BIN"; do
  [ -x "$b" ] || { echo "FAIL: not executable: $b" >&2; exit 3; }
done
if [ "$(sha256sum "$BASE_BIN" | cut -d' ' -f1)" = "$(sha256sum "$CAND_BIN" | cut -d' ' -f1)" ]; then
  echo "FAIL: baseline and candidate are the SAME binary; the A/B would be vacuous" >&2
  exit 4
fi

echo "== host =="
echo "  $(hostname)  kernel $(uname -r)  loadavg $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  metric: eventfd write/op (counted; denominator ops=2N exact)"
echo "  W=$W rounds=$ROUNDS n=$N per test"

measure() {  # BIN PORT EXTRA... -> write/op on stdout
  local bin="$1" port="$2"; shift 2
  taskset -c "$SERVER_CPUS" "$bin" --port "$port" \
    --experimental-sharded-set-get-workers "$W" >/dev/null 2>&1 &
  local sp=$!
  local up=0 i
  for i in $(seq 1 40); do
    "$CLI" -p "$port" ping >/dev/null 2>&1 && { up=1; break; }
    sleep 0.25
  done
  if [ "$up" -ne 1 ]; then kill -9 $sp 2>/dev/null || true; echo "0"; return; fi
  # Provenance: hash the image the kernel actually mapped for THIS process.
  local sha
  sha=$(sha256sum /proc/$sp/exe 2>/dev/null | cut -d' ' -f1)
  local out writes
  out=$(sudo -n perf stat -e syscalls:sys_enter_write -p $sp -x, -- \
        taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -t set,get -n "$N" \
        -c 128 -P 16 -r 100000 --threads 8 2>&1 >/dev/null)
  writes=$(echo "$out" | awk -F, '/sys_enter_write/{print $1}')
  kill -9 $sp 2>/dev/null || true
  wait $sp 2>/dev/null || true
  echo "${writes:-0} $sha"
}

RES=/tmp/azm_wake_ab.tsv; : > "$RES"
printf '\n%-6s %14s %14s %14s %10s %10s\n' round 'base w/op' 'cand w/op' 'null w/op' 'cand/base' 'null/base'
for r in $(seq 1 "$ROUNDS"); do
  case $((r % 3)) in
    1) ORDER="base cand null" ;;
    2) ORDER="cand null base" ;;
    0) ORDER="null base cand" ;;
  esac
  bw=0; cw=0; nw=0; bsha=""; csha=""
  for arm in $ORDER; do
    case "$arm" in
      base) read -r bw bsha <<<"$(measure "$BASE_BIN" 27891)" ;;
      cand) read -r cw csha <<<"$(measure "$CAND_BIN" 27892)" ;;
      null) read -r nw _     <<<"$(measure "$BASE_BIN" 27893)" ;;
    esac
  done
  awk -v r="$r" -v b="$bw" -v c="$cw" -v n="$nw" -v ops="$((2*N))" -v out="$RES" 'BEGIN{
    bo=b/ops; co=c/ops; no=n/ops;
    printf "%-6s %14.6f %14.6f %14.6f %10.4f %10.4f\n", r, bo, co, no,
           (bo>0)?co/bo:0, (bo>0)?no/bo:0;
    printf "%s\t%.8f\t%.8f\t%.8f\n", r, bo, co, no >> out;
  }'
  [ -n "$bsha" ] && echo "         base running-image ${bsha:0:16}  cand running-image ${csha:0:16}"
done

python3 - "$RES" <<'PY'
import sys, statistics as st
from pathlib import Path
sys.path.insert(0, str(Path("scripts").resolve()))
from perf_baseline_capture import _bootstrap_median_ci
rows = [l.split() for l in open(sys.argv[1]) if len(l.split()) == 4]
base = [float(r[1]) for r in rows]; cand = [float(r[2]) for r in rows]; nul = [float(r[3]) for r in rows]
eff = [c/b for c, b in zip(cand, base) if b > 0]
nl  = [n/b for n, b in zip(nul, base) if b > 0]
if not eff:
    print("no usable rows"); sys.exit(1)
elo, ehi = _bootstrap_median_ci(eff); nlo, nhi = _bootstrap_median_ci(nl)
em, nm = st.median(eff), st.median(nl)
print()
print(f"  A/B effect cand/base write/op = {em:.4f}  bootstrap 95% median CI [{elo:.4f}, {ehi:.4f}]")
print(f"  A/A null  null/base write/op = {nm:.4f}  bootstrap 95% median CI [{nlo:.4f}, {nhi:.4f}]")
dev_null = max(abs(nhi-1), abs(nlo-1), 1e-9)
sep = ehi < nlo or elo > nhi
print(f"  median base write/op {st.median(base):.6f} -> cand {st.median(cand):.6f} "
      f"({st.median(base)/max(st.median(cand),1e-12):.1f}x fewer)")
if sep and abs(em-1) >= 2*dev_null:
    print(f"  -> DECIDABLE: effect clears 2x the null's worst deviation "
          f"({abs(em-1)/dev_null:.1f}x)")
elif sep:
    print("  -> THIN: separated from the null but under the 2x margin")
else:
    print("  -> INSIDE THE NULL: not decidable")
print("  Gate: bootstrap median-CI decision with a 2x null margin. "
      "CV is provenance only and did not influence this verdict.")
PY
