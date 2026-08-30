#!/usr/bin/env bash
# Retired instructions (Ir) for ONE fixed job on ONE fr-server binary, via callgrind.
#
# WHY THIS EXISTS
# ---------------
# `scripts/instr_per_op_vs_redis.sh` is the instruction-count harness of record, but it
# needs perf counters. Where `kernel.perf_event_paranoid >= 3` an unprivileged user gets
# none, and that script now refuses to run rather than print zeros (see its preflight).
# Callgrind needs NO perf counters, is deterministic, and does not care what else the
# host is doing -- the same job retires the same instructions whether or not a neighbour
# is compiling. On a box that carries other tenants that is often the only trustworthy
# instruction number available.
#
# Used this way for `frankenredis-uhthd` (commit c0458c737) to price two keyspace levers
# at 0.03-0.11 pct run-to-run spread: the chunked node arena at +0.60 pct Ir and inline
# small keys at -1.38 pct.
#
# THE TRAP THIS ENCODES, WHICH COST A WRONG MEASUREMENT FIRST
# ----------------------------------------------------------
# The job must be IDENTICAL across arms or the Ir comparison is meaningless. The first
# version used `redis-benchmark -t get,set`, and the SET arm mints random keys: `DBSIZE`
# came out 32622 instead of 20000, i.e. the two arms had not run the same work at all.
# The job is therefore READ-ONLY GET over the exact populated keyspace, and DBSIZE is
# printed on every run so the reader can see the arms matched.
#
# HONEST LIMITS
#   * Ir is USER-SPACE compute. It does not price the syscall path, which on this host is
#     the majority of a redis-benchmark profile. Do not read it as throughput.
#   * Callgrind serialises and slows execution by ~50x. Absolute times mean nothing here;
#     only the Ir ratio between two arms does.
#   * Run each arm TWICE. The repeat pair IS the instrument's A/A null, and a difference
#     smaller than that spread is not a result.
#
# USAGE
#   scripts/callgrind_ir_fixed_job.sh <fr-binary> <port> <tag> [keys] [bench-ops]
#
# Compare two binaries by running it twice per arm and reading the four Ir values.
set -euo pipefail

BIN="${1:?usage: $0 <fr-binary> <port> <tag> [keys] [bench-ops]}"
PORT="${2:?port required}"
TAG="${3:?tag required}"
KEYS="${4:-20000}"
OPS="${5:-20000}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"

[ -x "$BIN" ]   || { echo "FAIL: $BIN not executable" >&2; exit 3; }
[ -x "$CLI" ]   || { echo "FAIL: missing $CLI" >&2; exit 3; }
[ -x "$BENCH" ] || { echo "FAIL: missing $BENCH" >&2; exit 3; }
command -v valgrind >/dev/null || { echo "FAIL: valgrind not installed" >&2; exit 3; }

WORK="$(mktemp -d)"
OUT="$WORK/callgrind.out"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

valgrind --tool=callgrind --callgrind-out-file="$OUT" --quiet \
  "$BIN" --port "$PORT" --dir "$WORK" --dbfilename "none_$TAG.rdb" \
  --save '' --appendonly no --enable-debug-command yes >/dev/null 2>&1 &
VPID=$!

# Callgrind start-up is slow; wait generously but bounded.
for _ in $(seq 1 900); do
  "$CLI" -p "$PORT" ping >/dev/null 2>&1 && break
  sleep 1
done
if ! "$CLI" -p "$PORT" ping >/dev/null 2>&1; then
  echo "FAIL[$TAG]: server never answered PING on port $PORT" >&2
  kill -9 "$VPID" 2>/dev/null || true
  exit 5
fi

"$CLI" -p "$PORT" debug populate "$KEYS" >/dev/null
"$BENCH" -p "$PORT" -t get -n "$OPS" -c 8 -P 1 -r "$KEYS" -q >/dev/null 2>&1
DB=$("$CLI" -p "$PORT" dbsize | tr -d '[:space:]')
"$CLI" -p "$PORT" shutdown nosave >/dev/null 2>&1 || true
wait "$VPID" 2>/dev/null || true

IR=$(grep -a "^summary:" "$OUT" | awk '{print $2}')
case "${IR:-}" in
  ''|*[!0-9]*)
    echo "FAIL[$TAG]: callgrind produced no Ir summary (got '${IR:-<nothing>}')" >&2
    exit 4
    ;;
esac
# DBSIZE is asserted, not merely printed: it is what proves two arms ran the same job.
if [ "$DB" != "$KEYS" ]; then
  echo "FAIL[$TAG]: dbsize=$DB, expected $KEYS -- the arms did not run the same job" >&2
  exit 6
fi

echo "$TAG dbsize=$DB Ir=$IR"
