#!/usr/bin/env bash
# partition_lock_futex_census.sh — COUNT the convoy instead of timing it.
#
# WHY A COUNT AND NOT A STOPWATCH
# -------------------------------
# The claim under test is mechanical: a hot key drives every reactor to PARK on
# one partition's futex, and the parking -- not the serialization -- is what
# costs. A count of `futex` entries per operation tests that directly and is
# immune to whatever else the host is running, which matters here because this
# box carries other tenants whose load swings the wall-clock arms by more than
# the effect. Wall-clock throughput answers "how fast"; this answers "why", and
# only the second one is trustworthy on a loaded host.
#
# THE ARMS
# --------
# Both are the SAME ELF, differing only by `FR_PARTITION_LOCK_SPINS`:
#   nospin (=0)  park immediately on contention -- the measured defect
#   spin   (=N)  bounded spin with backoff before parking
# Each is driven by an IDENTICAL fixed-size job, so futex-per-op is comparable
# without any timing assumption at all.
#
# Two fixtures, because the count only means something in contrast:
#   hotkey     -t lpush,lpop,hset  -- redis-benchmark points all three at ONE key
#   scattered  -t set,get,incr     -- keys ranged over -r, so partitions spread
# The prediction the spin has to satisfy: futex/op collapses on hotkey and does
# not move on scattered. A spin that also churns scattered is buying nothing and
# burning cores.
set -euo pipefail

CONNS="${CONNS:-64}"
TOTAL="${TOTAL:-200000}"
PIPE="${PIPE:-1}"
KEYSPACE="${KEYSPACE:-100000}"
WORKERS="${WORKERS:-16}"
CLIENT_THREADS="${CLIENT_THREADS:-8}"
SPIN_ON="${SPIN_ON:-48}"
FR_BIN="${FR_BIN:-/data/tmp/cargo-target-tpc/release/frankenredis}"
SERVER_CPUS="${SERVER_CPUS:-0-15,32-47}"
CLIENT_CPUS="${CLIENT_CPUS:-16-31,48-63}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
SPIN_PORT=27861; NOSPIN_PORT=27862

[ -x "$FR_BIN" ] || { echo "FAIL: $FR_BIN not executable" >&2; exit 3; }
command -v perf >/dev/null || { echo "FAIL: perf not installed" >&2; exit 3; }

echo "== host identity =="
echo "  host      $(hostname)   kernel $(uname -r)   loadavg $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  fr ELF    $(sha256sum "$FR_BIN" | cut -d' ' -f1)"
echo "  job       n=$TOTAL per family (3 families), c=$CONNS, P=$PIPE, r=$KEYSPACE, reactors=$WORKERS"
echo

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

FR_PARTITION_LOCK_SPINS="$SPIN_ON" taskset -c "$SERVER_CPUS" "$FR_BIN" --port $SPIN_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_fx_spin.log 2>&1 &
SPIN_PID=$!; PIDS+=($SPIN_PID)
FR_PARTITION_LOCK_SPINS=0 taskset -c "$SERVER_CPUS" "$FR_BIN" --port $NOSPIN_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_fx_nospin.log 2>&1 &
NOSPIN_PID=$!; PIDS+=($NOSPIN_PID)
sleep 2
for p in $SPIN_PORT $NOSPIN_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: no PONG on $p"; exit 6; }
done
S1=$(sha256sum /proc/$SPIN_PID/exe | cut -d' ' -f1)
S2=$(sha256sum /proc/$NOSPIN_PID/exe | cut -d' ' -f1)
[ "$S1" = "$S2" ] || { echo "FAIL: arms are not the same ELF"; exit 7; }
echo "  both arms RUNNING-IMAGE sha256: $S1"
echo "  (identical ELF; the only difference is FR_PARTITION_LOCK_SPINS)"
echo

# perf-stat the SERVER for the duration of one fixed job, then divide by the
# known op count. Counting events on the server pid attributes only the server's
# own futex traffic, not the client's.
census() {
  local pid="$1" port="$2" tests="$3" label="$4" ops=$((TOTAL * 3)) out
  out=$(mktemp)
  perf stat -e syscalls:sys_enter_futex,instructions:u -p "$pid" -x, -o "$out" -- \
    taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -t "$tests" -n "$TOTAL" \
      -c "$CONNS" -P "$PIPE" -r "$KEYSPACE" --threads "$CLIENT_THREADS" >/dev/null 2>&1
  local futex instr
  futex=$(awk -F, '/sys_enter_futex/{print $1}' "$out")
  instr=$(awk -F, '/instructions/{print $1}' "$out")
  rm -f "$out"
  awk -v f="${futex:-0}" -v i="${instr:-0}" -v n="$ops" -v l="$label" 'BEGIN{
    printf "  %-22s futex/op %10.4f    instr/op %12.1f\n", l, f/n, i/n
  }'
}

for FIXTURE in hotkey scattered; do
  if [ "$FIXTURE" = hotkey ]; then TESTS="lpush,lpop,hset"; else TESTS="set,get,incr"; fi
  echo "== $FIXTURE  ($TESTS) =="
  # Warm both arms so first-touch allocation is not counted as convoy.
  "$BENCH" -p $SPIN_PORT   -t "$TESTS" -n 20000 -c "$CONNS" -P "$PIPE" -r "$KEYSPACE" >/dev/null 2>&1
  "$BENCH" -p $NOSPIN_PORT -t "$TESTS" -n 20000 -c "$CONNS" -P "$PIPE" -r "$KEYSPACE" >/dev/null 2>&1
  census "$NOSPIN_PID" "$NOSPIN_PORT" "$TESTS" "nospin (park at once)"
  census "$SPIN_PID"   "$SPIN_PORT"   "$TESTS" "spin  ($SPIN_ON, backoff)"
  # Repeat with arm order swapped: a count should not care, and if it does the
  # host was doing something to one arm that it did not do to the other.
  census "$SPIN_PID"   "$SPIN_PORT"   "$TESTS" "spin   (order swapped)"
  census "$NOSPIN_PID" "$NOSPIN_PORT" "$TESTS" "nospin (order swapped)"
  echo
done
