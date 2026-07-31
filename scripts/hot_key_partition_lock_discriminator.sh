#!/usr/bin/env bash
# hot_key_partition_lock_discriminator.sh — separate the two things the mixed
# connection sweep found, and test the fix for the bad one.
#
# WHAT THE SWEEP FOUND (2026-07-31, 24 reactors, P=1)
# ---------------------------------------------------
# The per-core-reactor keyspace-partition topology splits cleanly by fixture:
#
#   SCATTERED families (set/get/incr, keys ranged over -r):
#     c=1 1.04x   c=8 1.50x   c=32 1.99-3.00x  vs live Redis
#   HOT-KEY families (lpush/lpop/hset -- redis-benchmark points ALL of these at
#   ONE fixed key, `mylist` / `myhash`):
#     c=1 1.03x   c=32 0.67x  c=64 0.33x   c=128 0.25x
#
# The hot-key arm was pinned at exactly 66,578 ops/s from c=8 upward while Redis
# scaled past 199,000. Monotone degradation with concurrency, at a FIXED absolute
# throughput, is the signature of a lock convoy: every reactor parks on one
# partition's futex, and the parking cost -- not the serialization -- is what is
# being paid. Serialization alone would flatten at Redis's single-thread rate,
# not below a third of it.
#
# WHAT THIS HARNESS SEPARATES
# ---------------------------
# Three axes, so no arm can absorb another's effect:
#   FIXTURE  scattered (-t set,get,incr) vs hot-key (-t lpush,lpop,hset)
#   SPIN     FR_PARTITION_LOCK_SPINS=0 (park immediately, the measured defect)
#            vs the shipped bounded spin-with-backoff
#   RIVAL    live redis-server, same cpuset, measured in the same invocation
#
# BOTH spin arms are the SAME ELF, distinguished only by an environment knob, so
# a spin-vs-no-spin verdict cannot be an artifact of two different binaries.
set -euo pipefail

CONNS="${CONNS:-32,64,128}"
ROUNDS="${ROUNDS:-5}"
TOTAL="${TOTAL:-300000}"
PIPE="${PIPE:-1}"
KEYSPACE="${KEYSPACE:-100000}"
WORKERS="${WORKERS:-24}"
REDIS_IO_THREADS="${REDIS_IO_THREADS:-8}"
CLIENT_THREADS="${CLIENT_THREADS:-8}"
FR_BIN="${FR_BIN:-/data/tmp/cargo-target-tpc/release/frankenredis}"
SERVER_CPUS="${SERVER_CPUS:-0-15,32-47}"
CLIENT_CPUS="${CLIENT_CPUS:-16-31,48-63}"
SPIN_ON="${SPIN_ON:-48}"

while [ $# -gt 0 ]; do
  case "$1" in
    -c) CONNS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -n) TOTAL="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -W) WORKERS="$2"; shift 2;;
    --bin) FR_BIN="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
SPIN_PORT=27851; NOSPIN_PORT=27852; RD_PORT=27853

[ -x "$FR_BIN" ] || { echo "FAIL: fr binary not executable: $FR_BIN" >&2; exit 3; }
for f in "$BENCH" "$REDIS" "$CLI"; do
  [ -x "$f" ] || { echo "FAIL: missing vendored $f" >&2; exit 3; }
done
for p in $SPIN_PORT $NOSPIN_PORT $RD_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 5; }
done

running_image_sha() { sha256sum /proc/"$1"/exe 2>/dev/null | cut -d' ' -f1; }
cpu_ticks() { awk '{print $14+$15}' /proc/"$1"/stat 2>/dev/null || echo 0; }
cpuset_physical() {
  echo "$1" | tr ',' '\n' | while read -r r; do
    case "$r" in *-*) seq "${r%%-*}" "${r##*-}" ;; *) echo "$r" ;; esac
  done | awk '{print $1 % 32}' | sort -u | grep -c .
}
CLIENT_PHYS=$(cpuset_physical "$CLIENT_CPUS")

echo "== host identity =="
echo "  host          $(hostname)   kernel $(uname -r)"
echo "  cpu           $(awk -F': ' '/model name/{print $2; exit}' /proc/cpuinfo) ($(nproc) threads)"
echo "  loadavg       $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  fr    ELF     $(sha256sum "$FR_BIN" | cut -d' ' -f1)"
echo "  redis ELF     $(sha256sum "$REDIS" | cut -d' ' -f1)"
echo "  server cpuset $SERVER_CPUS   client cpuset $CLIENT_CPUS ($CLIENT_PHYS physical)"
echo "  reactors      $WORKERS   n=$TOTAL per family, P=$PIPE, r=$KEYSPACE, rounds=$ROUNDS"
echo

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

taskset -c "$SERVER_CPUS" "$REDIS" --port $RD_PORT --save '' --appendonly no \
    --io-threads "$REDIS_IO_THREADS" --io-threads-do-reads yes >/tmp/fr_hk_redis.log 2>&1 &
RD_PID=$!; PIDS+=($RD_PID)
FR_PARTITION_LOCK_SPINS="$SPIN_ON" taskset -c "$SERVER_CPUS" "$FR_BIN" --port $SPIN_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_hk_spin.log 2>&1 &
SPIN_PID=$!; PIDS+=($SPIN_PID)
FR_PARTITION_LOCK_SPINS=0 taskset -c "$SERVER_CPUS" "$FR_BIN" --port $NOSPIN_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_hk_nospin.log 2>&1 &
NOSPIN_PID=$!; PIDS+=($NOSPIN_PID)
sleep 2
for p in $SPIN_PORT $NOSPIN_PORT $RD_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: no PONG on $p"; exit 6; }
done
echo "  spin arm   RUNNING-IMAGE sha256: $(running_image_sha $SPIN_PID)"
echo "  nospin arm RUNNING-IMAGE sha256: $(running_image_sha $NOSPIN_PID)"
echo "  redis      RUNNING-IMAGE sha256: $(running_image_sha $RD_PID)"
[ "$(running_image_sha $SPIN_PID)" = "$(running_image_sha $NOSPIN_PID)" ] \
  || { echo "FAIL: spin arms are not the same ELF"; exit 7; }
echo "  (spin and nospin arms confirmed identical ELF; they differ only by FR_PARTITION_LOCK_SPINS)"
echo

# Returns "ops/s client_cpu_pct" for one family group on one port.
run_group() {
  local port="$1" tests="$2" conns="$3" threads="$4" nfam="$5" t0 t1 c1 bp
  t0=$(date +%s.%N)
  taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -t "$tests" -n "$TOTAL" \
      -c "$conns" -P "$PIPE" -r "$KEYSPACE" --threads "$threads" >/dev/null 2>&1 &
  bp=$!
  while kill -0 $bp 2>/dev/null; do c1=$(cpu_ticks $bp); sleep 0.2; done
  wait $bp 2>/dev/null || true
  t1=$(date +%s.%N)
  awk -v s="$t0" -v e="$t1" -v n="$TOTAL" -v k="$nfam" -v cc="${c1:-0}" 'BEGIN{
    d=e-s; printf "%.0f %.0f", (d>0)?(k*n)/d:0, (d>0)?cc/(100*d)*100:0
  }'
}

RES=/tmp/fr_hot_key_discriminator.tsv; : > "$RES"
printf '%-10s %-6s %-6s %11s %11s %11s %9s %9s\n' \
  fixture conns round 'spin ops/s' 'nospin' 'redis' 'spin/rd' 'spin/nosp'

for FIXTURE in scattered hotkey; do
  if [ "$FIXTURE" = scattered ]; then TESTS="set,get,incr"; else TESTS="lpush,lpop,hset"; fi
  NFAM=3
  for C in ${CONNS//,/ }; do
    THREADS=$CLIENT_THREADS
    [ "$C" -lt "$CLIENT_THREADS" ] && THREADS=$C
    for r in $(seq 1 "$ROUNDS"); do
      # Rotate arm order by round so no arm always runs on a warm or a cold host.
      case $((r % 3)) in
        1) read -r s s_c <<<"$(run_group $SPIN_PORT   "$TESTS" $C $THREADS $NFAM)"
           read -r x x_c <<<"$(run_group $NOSPIN_PORT "$TESTS" $C $THREADS $NFAM)"
           read -r d d_c <<<"$(run_group $RD_PORT     "$TESTS" $C $THREADS $NFAM)";;
        2) read -r x x_c <<<"$(run_group $NOSPIN_PORT "$TESTS" $C $THREADS $NFAM)"
           read -r d d_c <<<"$(run_group $RD_PORT     "$TESTS" $C $THREADS $NFAM)"
           read -r s s_c <<<"$(run_group $SPIN_PORT   "$TESTS" $C $THREADS $NFAM)";;
        0) read -r d d_c <<<"$(run_group $RD_PORT     "$TESTS" $C $THREADS $NFAM)"
           read -r s s_c <<<"$(run_group $SPIN_PORT   "$TESTS" $C $THREADS $NFAM)"
           read -r x x_c <<<"$(run_group $NOSPIN_PORT "$TESTS" $C $THREADS $NFAM)";;
      esac
      rd=$(awk -v a="$s" -v b="$d" 'BEGIN{printf "%.4f", (b>0)?a/b:0}')
      sp=$(awk -v a="$s" -v b="$x" 'BEGIN{printf "%.4f", (b>0)?a/b:0}')
      printf '%-10s %-6s %-6s %11d %11d %11d %8sx %8sx  clt=%s%%\n' \
        "$FIXTURE" "$C" "$r" "$s" "$x" "$d" "$rd" "$sp" "$s_c"
      printf '%s\t%s\t%s\t%s\t%s\n' "$FIXTURE" "$C" "$s" "$x" "$d" >> "$RES"
    done
  done
done

echo
python3 - "$RES" "$CLIENT_PHYS" <<'PY'
import sys, statistics, random
rows = [l.split() for l in open(sys.argv[1]) if l.strip()]
by = {}
for fixture, c, s, x, d in rows:
    by.setdefault((fixture, int(c)), []).append((float(s), float(x), float(d)))
random.seed(20260731)

def ci(obs, num, den):
    boots = []
    for _ in range(2000):
        smp = [random.choice(obs) for _ in obs]
        md = statistics.median([o[den] for o in smp])
        boots.append(statistics.median([o[num] for o in smp]) / md if md else 0.0)
    boots.sort()
    return boots[int(0.025 * len(boots))], boots[int(0.975 * len(boots))]

print(f"{'fixture':>10} {'conns':>6} {'spin':>10} {'nospin':>10} {'redis':>10} "
      f"{'spin/redis':>12} {'95% CI':>18} {'spin/nospin':>12}")
for key in sorted(by):
    fixture, c = key
    obs = by[key]
    med = [statistics.median([o[i] for o in obs]) for i in range(3)]
    r_rd = med[0] / med[2] if med[2] else 0.0
    r_sp = med[0] / med[1] if med[1] else 0.0
    lo, hi = ci(obs, 0, 2)
    print(f"{fixture:>10} {c:>6} {med[0]:>10.0f} {med[1]:>10.0f} {med[2]:>10.0f} "
          f"{r_rd:>11.4f}x [{lo:>6.4f},{hi:>6.4f}] {r_sp:>11.4f}x")
PY
