#!/usr/bin/env bash
# parallel_heavy_keys.sh — MANY heavy keys at once, which is the thing a
# single-threaded incumbent structurally cannot do.
#
# THE DIFFERENCE FROM head_of_line.sh
# -----------------------------------
# head_of_line.sh puts ONE heavy key under light GET traffic and asks how much
# the light traffic degrades. That isolates blocking, but it caps the win at
# "we don't stall", because only one core is ever doing heavy work.
#
# This asks the other question: when EVERY client wants heavy work on a
# DIFFERENT key, how much total work does each engine get through? redis-server
# executes commands on one thread, so N heavy commands cost N x their duration
# no matter how many cores or io-threads exist -- `io-threads` parallelizes
# socket I/O only, never execution. A partitioned keyspace runs them on as many
# reactors as there are partitions holding the keys. This is where the gap
# should be a MULTIPLE rather than a margin.
#
# WHY BITCOUNT
# ------------
# O(N) in COMPUTE, returns ONE integer. So the measurement is execution
# parallelism and not reply size, socket writes, or allocator behaviour. LRANGE
# runs as a second fixture precisely because it does carry a large reply, so the
# two together separate "we execute in parallel" from "we also ship bytes".
#
# INTEGRITY GATES (this repo has been burned by all three)
#   * redis-benchmark counts an ERROR reply as a completed request. A server
#     that rejects the command instantly looks infinitely fast. So every arm is
#     probed on a raw socket first and must return the RIGHT REPLY TYPE, and the
#     probe reply is printed.
#   * A refused seed once made fr measure BITCOUNT over an EMPTY key and report
#     a meaningless 1.00x. So each engine prints a SERVER-SIDE measurement of
#     the seeded state (STRLEN/LLEN) and the run aborts if it is wrong.
#   * A second fr at identical config is the A/A null, so the reading carries
#     its own noise scale. On this shared box the null has been seen at 1.40x;
#     any ratio inside the null's margin is not a result.
set -euo pipefail

KEYS="${KEYS:-128}"            # distinct heavy keys, spread over partitions
VALBYTES="${VALBYTES:-1048576}"  # per-key string size for the BITCOUNT fixture
LISTLEN="${LISTLEN:-2000}"     # per-key list length for the LRANGE fixture
N="${N:-40000}"
CONNS="${CONNS:-64}"
ROUNDS="${ROUNDS:-3}"
WORKERS="${WORKERS:-16}"
CLIENT_THREADS="${CLIENT_THREADS:-8}"
REDIS_IO_THREADS="${REDIS_IO_THREADS:-8}"
FR_BIN="${FR_BIN:-/data/tmp/cargo-target/release/frankenredis}"
SERVER_CPUS="${SERVER_CPUS:-0-15,32-47}"
CLIENT_CPUS="${CLIENT_CPUS:-16-31,48-63}"
FIXTURES="${FIXTURES:-bitcount lrange}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_PORT="${FR_PORT:-27881}"; RD_PORT="${RD_PORT:-27882}"; FR2_PORT="${FR2_PORT:-27883}"

[ -x "$FR_BIN" ] || { echo "FAIL: $FR_BIN not executable" >&2; exit 3; }
for f in "$BENCH" "$REDIS" "$CLI"; do
  [ -x "$f" ] || { echo "FAIL: missing vendored $f" >&2; exit 3; }
done
for p in $FR_PORT $RD_PORT $FR2_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 5; }
done

cpuset_physical() {
  echo "$1" | tr ',' '\n' | while read -r r; do
    case "$r" in *-*) seq "${r%%-*}" "${r##*-}" ;; *) echo "$r" ;; esac
  done | awk '{print $1 % 32}' | sort -u | grep -c .
}

echo "== host identity =="
echo "  host      $(hostname)   kernel $(uname -r)   loadavg $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  cpu       $(awk -F': ' '/model name/{print $2; exit}' /proc/cpuinfo) ($(nproc) threads)"
echo "  fr ELF    $(sha256sum "$FR_BIN" | cut -d' ' -f1)"
echo "  redis ELF $(sha256sum "$REDIS" | cut -d' ' -f1)"
echo "  server    $SERVER_CPUS ($(cpuset_physical "$SERVER_CPUS") physical)   client $CLIENT_CPUS ($(cpuset_physical "$CLIENT_CPUS") physical)"
echo "  fixture   $KEYS keys, ${VALBYTES}B strings / ${LISTLEN}-element lists, n=$N c=$CONNS"
echo "  fr        --experimental-sharded-set-get-workers $WORKERS"
echo "  redis     --io-threads $REDIS_IO_THREADS --io-threads-do-reads yes"
echo

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_phk_fr.log 2>&1 &
FR_PID=$!; PIDS+=($FR_PID)
taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR2_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_phk_fr2.log 2>&1 &
FR2_PID=$!; PIDS+=($FR2_PID)
taskset -c "$SERVER_CPUS" "$REDIS" --port $RD_PORT --save '' --appendonly no \
    --io-threads "$REDIS_IO_THREADS" --io-threads-do-reads yes >/tmp/fr_phk_rd.log 2>&1 &
RD_PID=$!; PIDS+=($RD_PID)
sleep 2
for p in $FR_PORT $RD_PORT $FR2_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "PREFLIGHT FAIL: no PONG on $p" >&2; exit 6; }
done
echo "  fr    RUNNING-IMAGE $(sha256sum /proc/$FR_PID/exe | cut -d' ' -f1)  (null $(sha256sum /proc/$FR2_PID/exe | cut -d' ' -f1 | cut -c1-16))"
echo "  redis RUNNING-IMAGE $(sha256sum /proc/$RD_PID/exe | cut -d' ' -f1)"
echo

# ---------------------------------------------------------------- seeding ----
# redis-benchmark substitutes __rand_int__ with a 12-digit zero-padded integer
# in [0,r), so the seeded key names must match that width exactly or every
# command lands on a MISSING key and measures nothing.
seed() { # $1=port $2=fixture
  local port="$1" fx="$2" i name
  for i in $(seq 0 $((KEYS - 1))); do
    name=$(printf '%s:%012d' "$fx" "$i")
    if [ "$fx" = bitcount ]; then
      "$CLI" -p "$port" SETRANGE "$name" $((VALBYTES - 1)) x >/dev/null
    else
      "$CLI" -p "$port" DEL "$name" >/dev/null 2>&1 || true
      "$CLI" -p "$port" RPUSH "$name" $(seq 1 "$LISTLEN") >/dev/null 2>&1 \
        || for _ in $(seq 1 "$LISTLEN"); do "$CLI" -p "$port" LPUSH "$name" v >/dev/null; done
    fi
  done
}

# The seed must be verified ON THE SERVER, per engine. A silently refused write
# is exactly how a previous run measured an empty key and reported a fake 1.00x.
verify_seed() { # $1=port $2=label $3=fixture -> prints measurement, exits on mismatch
  local port="$1" label="$2" fx="$3" probe want got
  probe=$(printf '%s:%012d' "$fx" 0)
  if [ "$fx" = bitcount ]; then
    got=$("$CLI" -p "$port" STRLEN "$probe"); want="$VALBYTES"
    echo "    $label  STRLEN $probe = $got (want $want)"
  else
    got=$("$CLI" -p "$port" LLEN "$probe"); want="$LISTLEN"
    echo "    $label  LLEN $probe = $got (want $want)"
  fi
  [ "$got" = "$want" ] || { echo "FAIL: $label seed wrong for $fx: got $got want $want" >&2; exit 8; }
}

# redis-benchmark counts an ERROR reply as a completed request, so a server that
# rejects the command looks infinitely fast. Assert the reply TYPE on a raw
# socket and print it.
probe_reply() { # $1=port $2=label $3=fixture
  python3 - "$1" "$2" "$3" "$(printf '%s:%012d' "$3" 0)" <<'PY'
import socket, sys
port, label, fx, key = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
s = socket.create_connection(("127.0.0.1", port)); s.settimeout(10)
args = ["BITCOUNT", key] if fx == "bitcount" else ["LRANGE", key, "0", "99"]
out = f"*{len(args)}\r\n".encode()
for a in args:
    out += b"$%d\r\n%s\r\n" % (len(a), a.encode())
s.sendall(out)
r = s.recv(256)
head = r[:1].decode(errors="replace")
want = ":" if fx == "bitcount" else "*"
print(f"    {label}  {' '.join(args[:2])} -> {r[:48]!r}")
if head != want:
    print(f"FAIL: {label} returned {head!r} for {fx}, expected {want!r} "
          f"(an error reply would be counted as a completed request)", file=sys.stderr)
    sys.exit(9)
PY
}

measure_at() { # $1=conns $2=port $3=fxcmd -> "rps p99ms"
  local conns="$1" port="$2" fxcmd="$3" threads="$CLIENT_THREADS" n="$N"
  # redis-benchmark requires at least one connection per client thread.
  [ "$conns" -lt "$threads" ] && threads="$conns"
  # The serial control would otherwise run the full job against a one-connection
  # redis, which at ~4k ops/s takes minutes. Scale it down; the RATIO is what
  # this arm exists for, not its absolute throughput.
  [ "$conns" -eq 1 ] && n=$(( N / 20 < 2000 ? 2000 : N / 20 ))
  taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -n "$n" -c "$conns" -P 1 \
      --threads "$threads" -r "$KEYS" --csv $fxcmd 2>/dev/null \
    | awk -F, 'NR==2{gsub(/"/,"",$2); gsub(/"/,"",$7); printf "%s %s", $2, $7}'
}

measure() { # $1=port $2=fxcmd -> "rps p99ms"
  measure_at "$CONNS" "$1" "$2"
}

RES=$(mktemp)
for FX in $FIXTURES; do
  if [ "$FX" = bitcount ]; then
    FXCMD="BITCOUNT bitcount:__rand_int__"
  else
    FXCMD="LRANGE lrange:__rand_int__ 0 99"
  fi
  echo "== fixture: $FX  ($FXCMD) =="
  echo "  seeding $KEYS keys on each engine..."
  for pp in "$FR_PORT fr" "$RD_PORT redis" "$FR2_PORT fr2"; do
    set -- $pp; seed "$1" "$FX"
  done
  echo "  server-side seed verification:"
  verify_seed "$FR_PORT" "fr   " "$FX"
  verify_seed "$RD_PORT" "redis" "$FX"
  verify_seed "$FR2_PORT" "fr2  " "$FX"
  echo "  raw-socket reply-type probe:"
  probe_reply "$FR_PORT" "fr   " "$FX"
  probe_reply "$RD_PORT" "redis" "$FX"
  echo
  # SERIAL CONTROL. At c=1 neither engine has anything to parallelize, so this
  # ratio is pure per-op execution speed. Dividing the c=$CONNS ratio by this one
  # separates "our BITCOUNT is faster" from "we ran many at once" -- without it a
  # fast single-key implementation would masquerade as parallelism.
  read -r s1 _ <<<"$(CONNS=1 measure_at 1 "$FR_PORT" "$FXCMD")"
  read -r s2 _ <<<"$(CONNS=1 measure_at 1 "$RD_PORT" "$FXCMD")"
  SERIAL_RATIO=$(awk -v x="$s1" -v y="$s2" 'BEGIN{printf "%.3f", (y>0)?x/y:0}')
  printf '  serial control c=1: fr=%s redis=%s -> %sx (per-op speed alone)\n' \
    "$s1" "$s2" "$SERIAL_RATIO"
  echo
  printf '  %-6s %11s %11s %11s %9s %10s %10s\n' rnd 'fr ops/s' 'redis ops/s' 'fr2(null)' 'fr/redis' 'fr p99' 'redis p99'
  : > "$RES"
  for r in $(seq 1 "$ROUNDS"); do
    if [ $((r % 2)) -eq 1 ]; then
      read -r a ap <<<"$(measure "$FR_PORT" "$FXCMD")"
      read -r b bp <<<"$(measure "$RD_PORT" "$FXCMD")"
      read -r c cp <<<"$(measure "$FR2_PORT" "$FXCMD")"
    else
      read -r b bp <<<"$(measure "$RD_PORT" "$FXCMD")"
      read -r a ap <<<"$(measure "$FR_PORT" "$FXCMD")"
      read -r c cp <<<"$(measure "$FR2_PORT" "$FXCMD")"
    fi
    printf '  %-6s %11s %11s %11s %8.3fx %10s %10s\n' "$r" "$a" "$b" "$c" \
      "$(awk -v x="$a" -v y="$b" 'BEGIN{print (y>0)?x/y:0}')" "$ap" "$bp"
    printf '%s\t%s\t%s\n' "$a" "$b" "$c" >> "$RES"
  done
  python3 - "$RES" "$SERIAL_RATIO" "$CONNS" <<'PY'
import sys, statistics
rows = [l.split() for l in open(sys.argv[1]) if l.strip()]
serial, conns = float(sys.argv[2]), int(sys.argv[3])
fr = [float(r[0]) for r in rows]; rd = [float(r[1]) for r in rows]; n2 = [float(r[2]) for r in rows]
mf, mr, mn = statistics.median(fr), statistics.median(rd), statistics.median(n2)
ratio = mf/mr if mr else 0.0
null = mn/mf if mf else 0.0
margin = 2*abs(null-1.0)
verdict = "WIN" if ratio > 1.0+margin else ("LOSS" if ratio < 1.0-margin else "null")
print(f"  median fr={mf:,.0f}  redis={mr:,.0f}  ratio={ratio:.3f}x  "
      f"A/A null={null:.3f} (margin +/-{margin:.3f})  => {verdict}")
if serial > 0:
    print(f"  decomposition: {ratio:.3f}x at c={conns}  /  {serial:.3f}x serial (c=1)"
          f"  =  {ratio/serial:.2f}x from EXECUTION PARALLELISM alone")
PY
  echo
done
rm -f "$RES"
echo "  Each fixture drives $KEYS DISTINCT keys, so the work spreads over partitions on fr"
echo "  and over exactly one execution thread on redis regardless of --io-threads."
