#!/usr/bin/env bash
# sharded_thread_symbol_split.sh — what are the shard workers actually EXECUTING?
#
# The per-thread census (scripts/sharded_serial_stage_census.sh) shows the worker
# threads burning roughly as many instructions per operation as normal mode
# spends on the WHOLE operation. That cannot all be the store hit -- a SET/GET
# against an in-memory map is a small fraction of a command's cost, and the event
# loop still does the read, the parse and the write. So either the counter is
# lying or most of the worker's instructions are not work.
#
# This resolves it by symbol. perf record over the server for a whole job, then
# report split by thread comm, so the shard workers' dominant cost centre is
# named rather than inferred. Read the `fr-set-get-shar` rows: if the top frames are
# channel receive / spin / park machinery, the handoff costs more than the work
# it hands off, and the parallel section has negative value.
set -euo pipefail

W="${W:-8}"
TOTAL="${TOTAL:-1000000}"
CLIENTS="${CLIENTS:-128}"
PIPE="${PIPE:-16}"
KEYSPACE="${KEYSPACE:-100000}"
CLIENT_THREADS="${CLIENT_THREADS:-6}"
FR_BIN="${FR_BIN:-/tmp/fr_census_956a5ab34}"
SERVER_CPUS="${SERVER_CPUS:-0-7,32-39}"
CLIENT_CPUS="${CLIENT_CPUS:-8-15,40-47}"
FR_PORT="${FR_PORT:-27851}"
OUTDIR="${OUTDIR:-/tmp/sharded_symbols}"
FREQ="${FREQ:-2999}"

while [ $# -gt 0 ]; do
  case "$1" in
    -W) W="$2"; shift 2;;
    -n) TOTAL="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
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
# A stripped binary silently reports every frame as an address, which reads as a
# flat profile and would make the workers look cheap.
SYMS=$(nm "$FR_BIN" 2>/dev/null | wc -l)
[ "$SYMS" -gt 1000 ] || { echo "FAIL: $FR_BIN has $SYMS symbols; profile would not resolve" >&2; exit 4; }
ss -ltn 2>/dev/null | grep -q ":$FR_PORT " && { echo "PREFLIGHT FAIL: port $FR_PORT bound" >&2; exit 5; }

mkdir -p "$OUTDIR"
if [ "$W" = 0 ]; then SHARD_ARGS=(); else SHARD_ARGS=(--experimental-sharded-set-get-workers "$W"); fi

echo "== thread/symbol split, W=$W =="
echo "  fr ELF   $(sha256sum "$FR_BIN" | cut -d' ' -f1)  ($SYMS symbols)"
echo "  job      SET then GET, n=$TOTAL each, c=$CLIENTS, P=$PIPE, r=$KEYSPACE"
echo "  sampling cycles:u at ${FREQ}Hz over the server process"

taskset -c "$SERVER_CPUS" "$FR_BIN" --port "$FR_PORT" "${SHARD_ARGS[@]}" >"$OUTDIR/fr_W$W.log" 2>&1 &
FR_PID=$!
trap 'kill -9 $FR_PID 2>/dev/null || true' EXIT
sleep 2
"$CLI" -p "$FR_PORT" ping >/dev/null 2>&1 || { echo "FAIL: arm did not come up"; tail -3 "$OUTDIR/fr_W$W.log"; exit 6; }
echo "  running-image sha256 $(sha256sum /proc/$FR_PID/exe | cut -d' ' -f1)"

DATA="$OUTDIR/perf_W$W.data"
perf record -F "$FREQ" -e cycles:u --call-graph=fp -o "$DATA" -p "$FR_PID" -- \
  taskset -c "$CLIENT_CPUS" "$BENCH" -p "$FR_PORT" -t set,get -n "$TOTAL" \
    -c "$CLIENTS" -P "$PIPE" -r "$KEYSPACE" --threads "$CLIENT_THREADS" \
    >"$OUTDIR/bench_W$W.log" 2>&1

kill -9 $FR_PID 2>/dev/null || true

echo
echo "-- self-time share by THREAD GROUP --"
perf report -i "$DATA" --no-children --sort comm --percentage absolute --stdio 2>/dev/null \
  | grep -E '^\s+[0-9]' | head -20

# The kernel truncates comm to 15 characters, so every fr-set-get-shard-N thread
# reports as the SAME comm and one filter covers the whole worker pool.
SHARD_COMM="fr-set-get-shar"
EVLOOP_COMM="$(basename "$FR_BIN" | cut -c1-15)"

echo
echo "-- top self-time symbols inside the SHARD WORKER threads ($SHARD_COMM) --"
perf report -i "$DATA" --no-children --sort symbol --percentage relative --stdio \
  --comms "$SHARD_COMM" 2>/dev/null | grep -E '^\s+[0-9]' | head -25 \
  || echo "  (no shard-worker samples; W=0 has no workers)"

echo
echo "-- top self-time symbols inside the EVENT LOOP thread ($EVLOOP_COMM) --"
perf report -i "$DATA" --no-children --sort symbol --percentage relative --stdio \
  --comms "$EVLOOP_COMM" 2>/dev/null | grep -E '^\s+[0-9]' | head -25

echo
echo "perf data: $DATA"
