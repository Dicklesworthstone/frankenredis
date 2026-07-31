#!/usr/bin/env bash
# sharded_batch_length_discriminator.sh — separate BATCH COLLAPSE from CONTENTION.
#
# THE CONFOUND THIS EXISTS TO REMOVE
# ----------------------------------
# Running the census against a single key (-r 1) makes the whole sharded loss
# vanish: at W=8 the event loop drops from 2392 to 1268 instructions/op and
# throughput rises from 471k to 897k. But that arm changes TWO things at once --
# consecutive commands go back to sharing a shard (so a job envelope carries 16
# of them again) AND only one worker is left active (so the shared completion
# channel has one producer). Either could be the cause, and they imply opposite
# verdicts: batch length is schedulable, cross-core contention is not.
#
# THE SEPARATION
# --------------
# crc16_slot() honours the {hashtag} rule, so `{t3}:key:__rand_int__` routes by
# "t3" alone while the actual key still ranges over the whole keyspace. That
# buys a workload with a REAL scattered keyspace and a CHOSEN shard, which the
# -r flag cannot express. Three arms, identical client structure throughout
# (8 processes x 16 connections x P16), differing only in which shard each
# process's keys route to:
#
#   SCATTER   no tag                  -> every command a fresh shard: batch ~1, W producers
#   PINNED    tag per process, all W  -> batch 16, ALL W workers active, W producers
#   SINGLE    same tag for everyone   -> batch 16, ONE worker active, 1 producer
#
#   PINNED vs SCATTER isolates batch length  (contention held equal: W producers)
#   PINNED vs SINGLE  isolates contention    (batch length held equal: 16)
#
# If PINNED recovers, the loss is an amortization-scheduling defect and the path
# is fixable. If PINNED stays slow, the wall is cross-core and it is not.
set -euo pipefail

W="${W:-8}"
TOTAL="${TOTAL:-250000}"      # per process, per phase; ops = PROCS * 2 * TOTAL
PROCS="${PROCS:-8}"
CLIENTS="${CLIENTS:-16}"      # per process; PROCS*CLIENTS matches the census c=128
PIPE="${PIPE:-16}"
KEYSPACE="${KEYSPACE:-100000}"
FR_BIN="${FR_BIN:-/tmp/fr_census_956a5ab34}"
SERVER_CPUS="${SERVER_CPUS:-0-7,32-39}"
CLIENT_CPUS="${CLIENT_CPUS:-8-15,40-47}"
FR_PORT="${FR_PORT:-27881}"
OUTDIR="${OUTDIR:-/tmp/sharded_discriminator}"
ROUNDS="${ROUNDS:-3}"

while [ $# -gt 0 ]; do
  case "$1" in
    -W) W="$2"; shift 2;;
    -n) TOTAL="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    --bin) FR_BIN="$2"; shift 2;;
    --port) FR_PORT="$2"; shift 2;;
    --out) OUTDIR="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
[ -x "$FR_BIN" ] || { echo "FAIL: no fr binary at $FR_BIN" >&2; exit 3; }
[ -x "$BENCH" ] || { echo "FAIL: missing $BENCH" >&2; exit 3; }
ss -ltn 2>/dev/null | grep -q ":$FR_PORT " && { echo "PREFLIGHT FAIL: port $FR_PORT bound" >&2; exit 5; }
mkdir -p "$OUTDIR"

# Hashtags whose crc16_slot lands on each shard. Computed with the SAME CRC-16/
# XMODEM %16384 the server uses. A tag that silently mis-routed would quietly
# turn the PINNED arm back into SCATTER and invert the verdict, so the generator
# self-tests against Redis's own published CLUSTER KEYSLOT values first and
# refuses to emit anything if the checksum disagrees.
mapfile -t TAGS < <(python3 - "$W" <<'PY'
import sys

POLY = 0x1021
TAB = []
for i in range(256):
    crc = i << 8
    for _ in range(8):
        crc = ((crc << 1) ^ POLY) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    TAB.append(crc)


def slot(key: bytes) -> int:
    crc = 0
    for b in key:
        crc = ((crc << 8) & 0xFF00) ^ TAB[((crc >> 8) ^ b) & 0xFF]
    return crc % 16384


for probe, expected in ((b"foo", 12182), (b"bar", 5061), (b"hello", 866)):
    got = slot(probe)
    if got != expected:
        raise SystemExit(f"CRC16 self-test failed: {probe!r} -> {got}, expected {expected}")

w = int(sys.argv[1])
found = {}
i = 0
while len(found) < w:
    tag = f"t{i}"
    s = slot(tag.encode()) % w
    found.setdefault(s, tag)
    i += 1
    if i > 200000:
        raise SystemExit("could not cover every shard")
for s in range(w):
    print(found[s])
PY
)
[ "${#TAGS[@]}" -eq "$W" ] || { echo "FAIL: got ${#TAGS[@]} tags for W=$W" >&2; exit 6; }

EVENTS="instructions:u,task-clock,context-switches"
perf stat -e 'syscalls:sys_enter_futex' -x, true >/dev/null 2>&1 \
  && EVENTS="$EVENTS,syscalls:sys_enter_futex,syscalls:sys_enter_write"

OPS=$((PROCS * 2 * TOTAL))
echo "== batch-length vs contention discriminator, W=$W =="
echo "  host       $(hostname)  loadavg $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  fr ELF     $(sha256sum "$FR_BIN" | cut -d' ' -f1)"
echo "  client     $PROCS processes x $CLIENTS connections x P=$PIPE, keyspace $KEYSPACE"
echo "  job        SET then GET, n=$TOTAL per process per phase => ops=$OPS EXACT"
echo "  tags       ${TAGS[*]}"
echo "  rounds     $ROUNDS per arm"
echo

FR_PID=""
trap '[ -n "$FR_PID" ] && kill -9 $FR_PID 2>/dev/null || true' EXIT

# The tagged arms use LONGER keys than the untagged one ({t3}:key:... vs key:...),
# which changes parse and hash cost on its own. So every arm is also measured in
# NORMAL mode, and each sharded arm is compared against the normal arm running
# the IDENTICAL key pattern. Comparing sharded-pinned against normal-scatter
# would credit sharding with a key-length difference.
start_server() {
  local w="$1" args=()
  [ "$w" = 0 ] || args=(--experimental-sharded-set-get-workers "$w")
  taskset -c "$SERVER_CPUS" "$FR_BIN" --port "$FR_PORT" "${args[@]}" >"$OUTDIR/fr_W$w.log" 2>&1 &
  FR_PID=$!
  sleep 2
  "$CLI" -p "$FR_PORT" ping >/dev/null 2>&1 \
    || { echo "FAIL: W=$w arm did not come up"; tail -3 "$OUTDIR/fr_W$w.log"; exit 7; }
  echo "W=$w running-image sha256 $(sha256sum /proc/"$FR_PID"/exe | cut -d' ' -f1)  threads $(awk '/^Threads:/{print $2}' /proc/"$FR_PID"/status)"
}

# One shell command per arm so perf can follow the server across all PROCS
# clients and close its window exactly when the last of them exits.
arm_script() {
  local arm="$1" f="$OUTDIR/arm_$arm.sh" i tag keypat
  {
    echo '#!/usr/bin/env bash'
    echo 'set -u'
    for i in $(seq 0 $((PROCS - 1))); do
      case "$arm" in
        scatter) keypat='key:__rand_int__' ;;
        pinned)  tag="${TAGS[$((i % W))]}"; keypat="{$tag}:key:__rand_int__" ;;
        single)  tag="${TAGS[0]}";          keypat="{$tag}:key:__rand_int__" ;;
      esac
      echo "( taskset -c $CLIENT_CPUS $BENCH -p $FR_PORT -n $TOTAL -c $CLIENTS -P $PIPE -r $KEYSPACE -q SET '$keypat' xxx >/dev/null 2>&1; \\"
      echo "  taskset -c $CLIENT_CPUS $BENCH -p $FR_PORT -n $TOTAL -c $CLIENTS -P $PIPE -r $KEYSPACE -q GET '$keypat' >/dev/null 2>&1 ) &"
    done
    echo 'wait'
  } > "$f"
  chmod +x "$f"
  echo "$f"
}

RES="$OUTDIR/discriminator.tsv"; : > "$RES"
for w in 0 "$W"; do
  start_server "$w"
  for arm in scatter pinned single; do
    script="$(arm_script "$arm")"
    for r in $(seq 1 "$ROUNDS"); do
      PERF_OUT="$OUTDIR/perf_W${w}_${arm}_r${r}.csv"
      t0=$(date +%s.%N)
      perf stat --per-thread -e "$EVENTS" -x, -o "$PERF_OUT" -p "$FR_PID" -- "$script"
      t1=$(date +%s.%N)
      elapsed=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.3f", b-a}')
      printf 'W%s-%s\t%s\t%s\t%s\t%s\n' "$w" "$arm" "$r" "$elapsed" "$PERF_OUT" "$FR_PID" >> "$RES"
      echo "  W=$w $arm round $r  elapsed ${elapsed}s  ops/s $(awk -v n="$OPS" -v d="$elapsed" 'BEGIN{printf "%.0f", (d>0)?n/d:0}')"
    done
  done
  kill -9 "$FR_PID" 2>/dev/null || true
  FR_PID=""
  sleep 1
done

echo
python3 "$ROOT/scripts/_sharded_serial_stage_stats.py" "$RES" "$OPS"
