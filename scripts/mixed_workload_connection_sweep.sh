#!/usr/bin/env bash
# mixed_workload_connection_sweep.sh — the claim that has to survive: a REALISTIC
# MIXED workload, swept over concurrency, whole-job, against a LIVE redis-server.
#
# WHAT IT MEASURES
# ----------------
# redis-server executes commands on ONE thread. This harness asks whether
# FrankenRedis's per-core reactor topology converts that structural fact into
# throughput on a job that mixes command families, as concurrency rises from a
# single connection to 128.
#
# The SHAPE is the finding. At c=1 there is no parallelism to exploit and the two
# engines should land near parity -- a large win there would mean the job is
# measuring something other than concurrency. The ratio should then open up as
# connections climb, and the point where it stops opening names whatever
# saturated first.
#
# THE JOB
# -------
# `-t set,get,incr,lpush,lpop,hset` -- every command family the per-core reactor
# path serves, run as one redis-benchmark invocation so the whole-job wall time
# covers the whole mix. NOTE the shape of redis-benchmark's own fixtures: set,
# get and incr randomize their key over `-r`, but lpush/lpop/hset all hammer ONE
# fixed key (`mylist`, `myhash`). Those three therefore land on a single keyspace
# partition and serialize across reactors. That is a hot-key worst case for a
# partitioned design and it is deliberately left in: per-command output below
# separates the scattered families from the hot-key ones so neither hides in the
# whole-job number.
#
# FAIRNESS
# --------
#   * Both server arms get the SAME cpuset, measured one at a time, never
#     concurrently -- this removes the 10-14% core-IDENTITY bias that has
#     invalidated readings in this repository before.
#   * redis-server gets its best honest configuration: its own documented scaling
#     knob `io-threads` with `--io-threads-do-reads yes`, persistence off. It
#     cannot use more cores for command EXECUTION; that is the structural fact
#     under test, not a handicap imposed here.
#   * A second FrankenRedis at identical configuration runs as the A/A null in
#     the same invocation, so the null tracks the effect's own scale.
#   * Arm order alternates per round so a warming or drifting host cannot favour
#     one engine.
#   * ACTUAL OBSERVED thread count and the RUNNING-IMAGE sha256 (hashed from
#     /proc/<pid>/exe, not from the path we launched) are recorded per arm.
set -euo pipefail

CONNS="${CONNS:-1,8,32,64,128}"
ROUNDS="${ROUNDS:-3}"
TOTAL="${TOTAL:-100000}"        # per command family
PIPE="${PIPE:-1}"               # 1 = unpipelined, the realistic default
KEYSPACE="${KEYSPACE:-100000}"
WORKERS="${WORKERS:-24}"        # fr reactor count
REDIS_IO_THREADS="${REDIS_IO_THREADS:-8}"
CLIENT_THREADS="${CLIENT_THREADS:-8}"
# Every family the per-core reactor path admits AND redis-benchmark can drive.
# `spop` and `mset` are deliberately absent: SPOP is nondeterministic and MSET is
# multi-key, so neither is admitted as partition-local work and both would be
# answered with an error -- which redis-benchmark would count as a completed
# request. probe_families() below enforces that, per family, on a raw socket.
TESTS="${TESTS:-set,get,incr,lpush,rpush,lpop,rpop,sadd,hset,zadd,zpopmin,lrange_300}"
FR_BIN="${FR_BIN:-/data/tmp/cargo-target/release/frankenredis}"
SERVER_CPUS="${SERVER_CPUS:-0-23,32-55}"
CLIENT_CPUS="${CLIENT_CPUS:-24-31,56-63}"

while [ $# -gt 0 ]; do
  case "$1" in
    -c) CONNS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -n) TOTAL="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
    -W) WORKERS="$2"; shift 2;;
    -t) TESTS="$2"; shift 2;;
    --io-threads) REDIS_IO_THREADS="$2"; shift 2;;
    --bin) FR_BIN="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_PORT="${FR_PORT:-27841}"; RD_PORT="${RD_PORT:-27842}"; FR2_PORT="${FR2_PORT:-27843}"

[ -x "$FR_BIN" ] || { echo "FAIL: fr binary not executable: $FR_BIN" >&2; exit 3; }
for f in "$BENCH" "$REDIS" "$CLI"; do
  [ -x "$f" ] || { echo "FAIL: missing vendored $f" >&2; exit 3; }
done
for p in $FR_PORT $RD_PORT $FR2_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 5; }
done

observed_threads() { awk '/^Threads:/{print $2}' /proc/"$1"/status 2>/dev/null || echo 0; }
running_image_sha() { sha256sum /proc/"$1"/exe 2>/dev/null | cut -d' ' -f1; }
cpu_ticks() { awk '{print $14+$15}' /proc/"$1"/stat 2>/dev/null || echo 0; }

# Physical-core count of a cpuset like "0-23,32-55": siblings c and c+32 share a
# core, so the ceiling is the count of DISTINCT (c mod 32) values.
cpuset_physical() {
  echo "$1" | tr ',' '\n' | while read -r r; do
    case "$r" in
      *-*) seq "${r%%-*}" "${r##*-}" ;;
      *)   echo "$r" ;;
    esac
  done | awk '{print $1 % 32}' | sort -u | grep -c .
}
CLIENT_PHYS=$(cpuset_physical "$CLIENT_CPUS")
SERVER_PHYS=$(cpuset_physical "$SERVER_CPUS")
NTESTS=$(echo "$TESTS" | tr ',' '\n' | grep -c .)

echo "== host identity =="
echo "  host          $(hostname)"
echo "  kernel        $(uname -r)"
echo "  cpu           $(awk -F': ' '/model name/{print $2; exit}' /proc/cpuinfo) ($(nproc) threads)"
echo "  loadavg       $(cut -d' ' -f1-3 /proc/loadavg)"
echo "  fr    ELF     $(sha256sum "$FR_BIN" | cut -d' ' -f1)"
echo "  redis ELF     $(sha256sum "$REDIS" | cut -d' ' -f1)"
echo "  server cpuset $SERVER_CPUS ($SERVER_PHYS physical)   client cpuset $CLIENT_CPUS ($CLIENT_PHYS physical)"
echo "  job           -t $TESTS, n=$TOTAL each ($NTESTS families), P=$PIPE, r=$KEYSPACE"
echo "  fr config     --experimental-sharded-set-get-workers $WORKERS"
echo "  redis config  --io-threads $REDIS_IO_THREADS --io-threads-do-reads yes --save '' --appendonly no"
echo

PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

taskset -c "$SERVER_CPUS" "$REDIS" --port $RD_PORT --save '' --appendonly no \
    --io-threads "$REDIS_IO_THREADS" --io-threads-do-reads yes \
    >/tmp/fr_mixed_redis.log 2>&1 &
RD_PID=$!; PIDS+=($RD_PID)
taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_mixed_fr.log 2>&1 &
FR_PID=$!; PIDS+=($FR_PID)
taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR2_PORT \
    --experimental-sharded-set-get-workers "$WORKERS" >/tmp/fr_mixed_fr2.log 2>&1 &
FR2_PID=$!; PIDS+=($FR2_PID)
sleep 2
for p in $FR_PORT $RD_PORT $FR2_PORT; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || {
    echo "PREFLIGHT FAIL: no PONG on port $p"; tail -5 /tmp/fr_mixed_fr.log; exit 6;
  }
done
echo "  redis RUNNING-IMAGE sha256: $(running_image_sha $RD_PID)  (threads at rest: $(observed_threads $RD_PID))"
echo "  fr    RUNNING-IMAGE sha256: $(running_image_sha $FR_PID)  (null arm: $(running_image_sha $FR2_PID))"
echo "  fr    startup line:         $(grep -m1 'ready' /tmp/fr_mixed_fr.log || true)"
echo

# INTEGRITY GATE. redis-benchmark counts an ERROR reply as a completed request,
# so an engine that REFUSES a command looks infinitely fast at it -- this repo
# has already published a "3.46-5.15x" that was a server answering -CROSSSHARD.
# The whole-job number here averages every selected family, so one refused family
# would inflate the total silently. Send each family's actual command shape on a
# raw socket and require a non-error reply from BOTH engines before measuring.
probe_families() {
  python3 - "$FR_PORT" "$RD_PORT" "$TESTS" <<'PY'
import socket, sys
fr_port, rd_port, tests = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3].split(',')
# The command shape redis-benchmark actually issues for each named test, taken
# from redis-benchmark.c. Fixed key names (mylist/myset/myhash/myzset) are the
# benchmark's own, and are why those families land on ONE partition.
SHAPES = {
    "set": ["SET", "key:probe", "v"], "get": ["GET", "key:probe"],
    "incr": ["INCR", "counter:probe"],
    "lpush": ["LPUSH", "mylist", "v"], "rpush": ["RPUSH", "mylist", "v"],
    "lpop": ["LPOP", "mylist"], "rpop": ["RPOP", "mylist"],
    "sadd": ["SADD", "myset", "element:probe"],
    "spop": ["SPOP", "myset"],
    "hset": ["HSET", "myhash", "element:probe", "v"],
    "zadd": ["ZADD", "myzset", "1", "element:probe"],
    "zpopmin": ["ZPOPMIN", "myzset"],
    "mset": ["MSET", "key:a", "v", "key:b", "v"],
}
for t in tests:
    SHAPES.setdefault(t.split('_')[0] if t.startswith('lrange') else t, None)
for t in tests:
    base = "lrange" if t.startswith("lrange") else t
    shape = SHAPES.get(base) or (["LRANGE", "mylist", "0", "99"] if base == "lrange" else None)
    if shape is None:
        print(f"FAIL: no probe shape known for family {t!r}", file=sys.stderr); sys.exit(9)
    out = f"*{len(shape)}\r\n".encode()
    for a in shape:
        out += b"$%d\r\n%s\r\n" % (len(a), a.encode())
    row = []
    for label, port in (("fr", fr_port), ("redis", rd_port)):
        s = socket.create_connection(("127.0.0.1", port)); s.settimeout(10)
        s.sendall(out)
        r = s.recv(256); s.close()
        row.append(f"{label}={r[:40]!r}")
        if r[:1] == b"-":
            print(f"    {t:<12} {' '.join(row)}")
            print(f"FAIL: {label} answered {t!r} with an ERROR; redis-benchmark would "
                  f"count it as a completed request and inflate the whole-job rate",
                  file=sys.stderr)
            sys.exit(9)
        if not r:
            print(f"FAIL: {label} closed the connection on {t!r}", file=sys.stderr); sys.exit(9)
    print(f"    {t:<12} {'  '.join(row)}")
PY
}
echo "  raw-socket reply probe (an error reply counts as a completed request):"
probe_families
echo

# Whole-job wall time over the WHOLE mix, plus per-family ops/s parsed from the
# csv output. Returns "ops/s server_cpu_pct client_cpu_pct" on stdout and writes
# the per-family csv to $3.
run_job() {
  local port="$1" server_pid="$2" csv="$3" conns="$4" t0 t1 s0 s1 c1 bp
  s0=$(cpu_ticks "$server_pid")
  t0=$(date +%s.%N)
  taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -t "$TESTS" -n "$TOTAL" \
      -c "$conns" -P "$PIPE" -r "$KEYSPACE" --threads "$CLIENT_THREADS" --csv \
      >"$csv" 2>/dev/null &
  bp=$!
  while kill -0 $bp 2>/dev/null; do
    c1=$(cpu_ticks $bp)
    sleep 0.2
  done
  wait $bp 2>/dev/null || true
  t1=$(date +%s.%N)
  s1=$(cpu_ticks "$server_pid")
  awk -v s="$t0" -v e="$t1" -v n="$TOTAL" -v k="$NTESTS" -v a="$s0" -v b="$s1" -v cc="${c1:-0}" 'BEGIN{
    d=e-s;
    printf "%.0f %.0f %.0f", (d>0)?(k*n)/d:0, (b-a)/(100*d)*100, cc/(100*d)*100
  }'
}

RES=/tmp/fr_mixed_connection_sweep.tsv; : > "$RES"
CLIENT_BOUND_ROWS=0
printf '%-6s %-6s %12s %12s %12s %9s %8s\n' conns round 'fr ops/s' 'redis ops/s' 'fr2(null)' 'fr/redis' 'obs_thr'

for C in ${CONNS//,/ }; do
  # redis-benchmark requires at least one connection per client thread.
  THREADS=$CLIENT_THREADS
  [ "$C" -lt "$CLIENT_THREADS" ] && THREADS=$C
  for r in $(seq 1 "$ROUNDS"); do
    if [ $((r % 2)) -eq 1 ]; then
      read -r a  a_scpu a_ccpu <<<"$(CLIENT_THREADS=$THREADS run_job $FR_PORT  $FR_PID  /tmp/fr_mixed_fr.csv    $C)"
      read -r b  b_scpu b_ccpu <<<"$(CLIENT_THREADS=$THREADS run_job $RD_PORT  $RD_PID  /tmp/fr_mixed_redis.csv $C)"
      read -r n2 n_scpu n_ccpu <<<"$(CLIENT_THREADS=$THREADS run_job $FR2_PORT $FR2_PID /tmp/fr_mixed_fr2.csv   $C)"
    else
      read -r b  b_scpu b_ccpu <<<"$(CLIENT_THREADS=$THREADS run_job $RD_PORT  $RD_PID  /tmp/fr_mixed_redis.csv $C)"
      read -r a  a_scpu a_ccpu <<<"$(CLIENT_THREADS=$THREADS run_job $FR_PORT  $FR_PID  /tmp/fr_mixed_fr.csv    $C)"
      read -r n2 n_scpu n_ccpu <<<"$(CLIENT_THREADS=$THREADS run_job $FR2_PORT $FR2_PID /tmp/fr_mixed_fr2.csv   $C)"
    fi
    thr=$(observed_threads $FR_PID)
    ratio=$(awk -v x="$a" -v y="$b" 'BEGIN{printf "%.4f", (y>0)?x/y:0}')
    # CLIENT-BOUND guard: if the client is near ITS ceiling while neither server
    # is, ops/s describes redis-benchmark, not a server.
    #
    # The ceiling is min(physical cores, CLIENT THREADS), not the cpuset alone.
    # redis-benchmark is threaded, so 8 --threads can never exceed 800% however
    # many cores the cpuset owns. Comparing 728% against a 16-core 1600% ceiling
    # read as 45% utilised and stayed silent, when against the real 800% ceiling
    # it was 91% and saturated -- which silently understated fr at c=64 and
    # c=128, where its throughput had gone flat (262k -> 290k) because the
    # CLIENT had stopped scaling, not the server.
    flag=$(awk -v cc="$a_ccpu" -v cb="$b_ccpu" -v cph="$CLIENT_PHYS" -v cth="$THREADS" \
               -v sa="$a_scpu" -v sb="$b_scpu" 'BEGIN{
      lim=(cph<cth)?cph:cth; ceil=lim*100;
      maxc=(cc>cb)?cc:cb; maxs=(sa>sb)?sa:sb;
      if (ceil>0 && maxc >= 0.85*ceil && maxs < 0.85*ceil) printf "CLIENT-BOUND"; else printf "ok";
    }')
    printf '%-6s %-6s %12d %12d %12d %9s %8s  srv=%s%%/%s%% clt=%s%% %s\n' \
      "$C" "$r" "$a" "$b" "$n2" "$ratio" "$thr" "$a_scpu" "$b_scpu" "$a_ccpu" "$flag"
    printf '%s\t%s\t%s\t%s\n' "$C" "$a" "$b" "$n2" >> "$RES"
    [ "$flag" = "CLIENT-BOUND" ] && CLIENT_BOUND_ROWS=$((CLIENT_BOUND_ROWS + 1))
  done
  # Per-family split from the LAST round of this connection count, so the
  # hot-key families (lpush/lpop/hset, all one key) stay visible next to the
  # scattered ones (set/get/incr) instead of averaging into the whole-job row.
  echo "        per-family ops/s at c=$C (fr | redis):"
  paste <(tail -n +2 /tmp/fr_mixed_fr.csv) <(tail -n +2 /tmp/fr_mixed_redis.csv) \
    | awk -F'\t' '{
        split($1,f,","); split($2,r,",");
        gsub(/"/,"",f[1]); gsub(/"/,"",f[2]); gsub(/"/,"",r[2]);
        printf "          %-10s %12.0f %12.0f   %.4fx\n", f[1], f[2], r[2], (r[2]>0)?f[2]/r[2]:0
      }' 2>/dev/null || true
done

echo
python3 - "$RES" <<'PY'
import sys, statistics, random
rows = [l.split() for l in open(sys.argv[1]) if l.strip()]
by = {}
for c, a, b, n2 in rows:
    by.setdefault(int(c), []).append((float(a), float(b), float(n2)))
random.seed(20260731)
print(f"{'conns':>6} {'fr med':>12} {'redis med':>12} {'ratio':>8} {'95% CI':>18} {'null':>8} {'verdict':>10}")
for c in sorted(by):
    obs = by[c]
    fr = [o[0] for o in obs]; rd = [o[1] for o in obs]; n2 = [o[2] for o in obs]
    ratio = statistics.median(fr) / statistics.median(rd) if statistics.median(rd) else 0.0
    null = statistics.median(n2) / statistics.median(fr) if statistics.median(fr) else 0.0
    boots = []
    for _ in range(2000):
        s = [random.choice(obs) for _ in obs]
        mr = statistics.median([x[1] for x in s])
        boots.append(statistics.median([x[0] for x in s]) / mr if mr else 0.0)
    boots.sort()
    lo, hi = boots[int(0.025*len(boots))], boots[int(0.975*len(boots))]
    # A/A null margin: the effect must clear twice the null arm's own deviation
    # from 1.0, so a noisy host cannot manufacture a verdict.
    margin = 2 * abs(null - 1.0)
    verdict = "WIN" if lo > 1.0 + margin else ("LOSS" if hi < 1.0 - margin else "null")
    print(f"{c:>6} {statistics.median(fr):>12.0f} {statistics.median(rd):>12.0f} "
          f"{ratio:>7.4f}x [{lo:>6.4f},{hi:>6.4f}] {null:>7.4f} {verdict:>10}")
PY

if [ "$CLIENT_BOUND_ROWS" -gt 0 ]; then
  echo
  echo "!! $CLIENT_BOUND_ROWS row(s) were CLIENT-BOUND: the client cpuset ($CLIENT_PHYS physical"
  echo "   cores) was near saturation while neither server was. Those rows measure"
  echo "   redis-benchmark, not a server. Re-run with a larger CLIENT_CPUS."
fi
