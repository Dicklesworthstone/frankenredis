#!/usr/bin/env bash
# mimalloc's COMMITTED bytes for one fr-server binary after a fixed key load.
#
# WHY THIS EXISTS
# ---------------
# `scripts/keyspace_ram_vs_redis.py` measures WHAT the keyspace costs (delta VmRSS vs a
# live Redis, with an A/A null). It cannot say why. This reads the allocator's own
# accounting, and the two agree: on `frankenredis-uhthd`, `committed` tracked delta RSS
# to about 1 pct on every build measured --
#
#     pre-arena       145.4 B/key RSS    144.5 MiB committed
#     chunked+inline  103.2 B/key RSS    101.4 MiB committed
#
# so COMMITTED IS THE QUANTITY A KEYSPACE RAM LEVER HAS TO MOVE, and it can be read
# directly instead of modelled.
#
# WHAT IT SETTLED, AND WHY THAT MATTERS MORE THAN THE NUMBER
# ----------------------------------------------------------
# Structural arithmetic (`capacity x size_of(node)`) was wrong every time it was used on
# that bead. Node width 72 -> 64 predicted -8 B/key and delivered ZERO at four key counts;
# this instrument showed why, by reporting an IDENTICAL committed figure for both builds
# (130.3 MiB) while a much wider node did move it (166.8 MiB). Committed is quantized at
# the size of the allocation actually requested, which also made `ARENA_CHUNK_SHIFT` worth
# sweeping -- it turned out to be worth 8.5 B/key (commit e5394d9e2).
#
# So: do not price a keyspace change by multiplying a struct width by the key count.
# Price it with keyspace_ram_vs_redis.py, and explain it with this.
#
# HONEST LIMITS
#   * Requires the SHIPPING build. The mimalloc feature is on by default; a
#     `--no-default-features` binary has no mimalloc and prints nothing.
#   * This mimalloc is built with terse stats: there is no per-size-class breakdown. A
#     build with MI_STAT=2 would print per-bin committed and could close the remaining
#     "which bin absorbs it" question. Name that instrument rather than guessing the rule.
#   * One process, no null. It explains a difference; it does not certify one. The
#     vs-Redis ratio of record still comes from keyspace_ram_vs_redis.py.
#
# USAGE
#   scripts/mimalloc_committed_after_load.sh <fr-binary> <port> <tag> [keys] [key-prefix]
set -euo pipefail

BIN="${1:?usage: $0 <fr-binary> <port> <tag> [keys] [key-prefix]}"
PORT="${2:?port required}"
TAG="${3:?tag required}"
KEYS="${4:-1000000}"
PREFIX="${5:-}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
[ -x "$BIN" ] || { echo "FAIL: $BIN not executable" >&2; exit 3; }
[ -x "$CLI" ] || { echo "FAIL: missing $CLI" >&2; exit 3; }

WORK="$(mktemp -d)"
LOG="$WORK/mimalloc.log"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

MIMALLOC_SHOW_STATS=1 "$BIN" --port "$PORT" --dir "$WORK" \
  --dbfilename "none_$TAG.rdb" --save '' --appendonly no \
  --enable-debug-command yes >"$LOG" 2>&1 &
VPID=$!
for _ in $(seq 1 120); do
  "$CLI" -p "$PORT" ping >/dev/null 2>&1 && break
  sleep 1
done
if ! "$CLI" -p "$PORT" ping >/dev/null 2>&1; then
  echo "FAIL[$TAG]: server never answered PING on port $PORT" >&2
  kill -9 "$VPID" 2>/dev/null || true
  exit 5
fi

EMPTY=$(awk '/VmRSS/{print $2}' "/proc/$VPID/status")
if [ -n "$PREFIX" ]; then
  "$CLI" -p "$PORT" debug populate "$KEYS" "$PREFIX" >/dev/null
else
  "$CLI" -p "$PORT" debug populate "$KEYS" >/dev/null
fi
LOADED=$(awk '/VmRSS/{print $2}' "/proc/$VPID/status")
DB=$("$CLI" -p "$PORT" dbsize | tr -d '[:space:]')
"$CLI" -p "$PORT" shutdown nosave >/dev/null 2>&1 || true
wait "$VPID" 2>/dev/null || true

if [ "$DB" != "$KEYS" ]; then
  echo "FAIL[$TAG]: dbsize=$DB, expected $KEYS -- not holding the intended keyspace" >&2
  exit 6
fi

COMMIT=$(awk '/^  committed/{print $3, $4; exit}' "$LOG")
PURGED=$(awk '/^  purged/{print $3, $4; exit}' "$LOG")
# No stats block means the binary is not linked against mimalloc; say so rather than
# print an empty field that reads as zero.
if [ -z "${COMMIT:-}" ]; then
  echo "FAIL[$TAG]: no mimalloc stats in output -- is this a --no-default-features build?" >&2
  exit 4
fi

DELTA_KB=$((LOADED - EMPTY))
printf '%s dbsize=%s deltaRSS=%s kB (%.1f B/key) committed=%s purged=%s\n' \
  "$TAG" "$DB" "$DELTA_KB" \
  "$(awk -v d="$DELTA_KB" -v k="$KEYS" 'BEGIN{print d*1024/k}')" \
  "$COMMIT" "${PURGED:-n/a}"
