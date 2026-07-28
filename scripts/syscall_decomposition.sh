#!/usr/bin/env bash
# syscall_decomposition.sh — decompose the pipelined fr-vs-redis throughput gap
# into (user instructions | kernel instructions | syscalls | wakeups) PER OPERATION.
#
# WHY THIS EXISTS
# ---------------
# The campaign brief asserts the P16 gap is a *submission-path* problem ("a syscall
# per poll cycle that Redis amortizes") and prescribes io_uring. This repo's own
# ledger (2026-07-24 cc BLOCKER) asserts the opposite — that fr already coalesces a
# whole 16-command batch into ONE write syscall. Both cannot be right, and neither
# was measured with syscall COUNTERS. This script measures it directly, cheaply, and
# without strace's ~100x interposition tax, using the raw_syscalls:sys_enter
# tracepoint plus the u/k instruction split.
#
# The decomposition is the profile-first attribution for any submission-path lever:
#   syscalls/op        -> is there anything for io_uring to amortize at all?
#   instructions:k/op  -> how much of the cost is *in* the kernel (copy, sched)?
#   instructions:u/op  -> the command path (already measured ahead of redis)
#   context-switches/s -> wakeup/latency cost the counters above do not capture
#
# Both engines are measured with the SAME fixed-work client, on dedicated cores,
# with the order of the two arms ALTERNATED per round, and the statistic is the
# MEDIAN of per-round ratios (bench-harness contract §2.2). An A/A null control
# (fr vs a second fr on another core) runs in the same invocation.
#
# Usage: scripts/syscall_decomposition.sh [-t set,get,incr,...] [-P pipeline]
#                                         [-c clients] [-s seconds] [-R rounds]
#                                         [--bin PATH]
# `-t` accepts a comma-separated list; the servers are started once and every
# workload is swept against the same three processes, so cross-workload rows are
# directly comparable and the process-startup cost is paid once.
set -euo pipefail

BENCH_T=set; PIPE=16; CLIENTS=50; SECS=6; ROUNDS=5; KEYSPACE=100000
FR_BIN="${FR_BIN:-/tmp/fr_azm_base}"
while [ $# -gt 0 ]; do
  case "$1" in
    -t) BENCH_T="$2"; shift 2;;
    -P) PIPE="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -s) SECS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    -r) KEYSPACE="$2"; shift 2;;
    --bin) FR_BIN="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REDIS_SERVER="$ROOT/legacy_redis_code/redis/src/redis-server"
REDIS_BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS_CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_PORT=27311; RD_PORT=27312; FR2_PORT=27313
# Cores are overridable because this is a SHARED host: peers pin their own
# head-to-head benchmarks, and a peer landing on the fr arm's core silently
# halves that arm. Defaults are a starting point, not a reservation.
FR_CORE="${FR_CORE:-40}"; RD_CORE="${RD_CORE:-41}"
FR2_CORE="${FR2_CORE:-42}"; CLIENT_CORE="${CLIENT_CORE:-44}"
# A SINGLE-CORE redis-benchmark saturates at ~75k ops/s while the server still
# has headroom (measured 2026-07-25: client 100.1% of a core, server 83.1%). Every
# unpipelined ops/s ratio taken that way measured the CLIENT, not the server, and
# no server-side lever can move such a number. Drive the client across several
# cores with --threads, and see the CLIENT-BOUND guard below.
CLIENT_CORES="${CLIENT_CORES:-$CLIENT_CORE}"
CLIENT_THREADS="${CLIENT_THREADS:-4}"
# Extra server args for the fr arms, e.g. FR_ARGS=--io-uring-output. Applied to
# BOTH fr arms so the A/A null stays a true same-configuration control.
FR_ARGS="${FR_ARGS:-}"

# --- core-contention preflight ----------------------------------------------
# A foreign process pinned to one arm's core does NOT look like noise: it looks
# like a clean, reproducible, wrong answer. On 2026-07-25 a peer's
# `cand-headtohead` sat on core 40 at 53% and drove the fr-vs-fr A/A null to
# 0.556 — two identical binaries differing 2x — while instructions:u/op stayed
# at 0.98, i.e. every throughput ratio from that window was invalid and every
# instruction ratio was fine. Refuse to start rather than emit that quietly.
check_core() {
  local core="$1" role="$2" busy
  busy=$(ps -eo psr,pcpu,comm --no-headers \
         | awk -v c="$core" '$1==c && $2>10 && $3!~/^(fr_azm|frankenredis|redis-server|redis-benchmark|perf)$/ {printf "%s(%.0f%%) ", $3, $2}')
  if [ -n "$busy" ]; then
    echo "PREFLIGHT FAIL: core $core ($role) is contended by: $busy" >&2
    echo "  Pick a free core, e.g.  FR_CORE=51 RD_CORE=52 FR2_CORE=54 CLIENT_CORE=47 $0 ..." >&2
    echo "  Free-ish cores right now:" >&2
    for c in $(seq 0 $(($(nproc) - 1))); do
      u=$(ps -eo psr,pcpu --no-headers | awk -v c="$c" '$1==c {s+=$2} END {printf "%.0f", s+0}')
      [ "$u" -lt 5 ] && printf '    core %s (%s%%)\n' "$c" "$u" >&2
    done | head -8
    exit 3
  fi
}
check_core "$FR_CORE" "fr arm"
check_core "$RD_CORE" "redis arm"
check_core "$FR2_CORE" "A/A null arm"
check_core "$CLIENT_CORE" "client"

# --- harness contract part 1: identify the exact ELF under test -------------
echo "== binaries under test =="
echo "fr    $(sha256sum "$FR_BIN")"
echo "redis $(sha256sum "$REDIS_SERVER")"
echo "host  $(hostname)  load $(cut -d' ' -f1-3 /proc/loadavg)"
echo

cleanup() {
  for p in $FR_PID $RD_PID $FR2_PID; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done
}
FR_PID=""; RD_PID=""; FR2_PID=""
trap cleanup EXIT

start_servers() {
  # A previous run killed mid-flight skips its EXIT trap and leaves its servers
  # bound. The new servers then fail to bind, the script happily measures the
  # STALE processes (which no client is driving), and every counter comes back
  # 0 — a silent zero, not an error. Refuse to start on a busy port.
  for p in $FR_PORT $RD_PORT $FR2_PORT; do
    if ss -ltn 2>/dev/null | grep -q ":$p "; then
      echo "PREFLIGHT FAIL: port $p is already bound — a previous run left servers behind." >&2
      echo "  Clear them first:" >&2
      echo "    for p in \$(ss -ltnp | grep -E ':2731[123]' | grep -oP 'pid=\\K[0-9]+' | sort -u); do kill -9 \$p; done" >&2
      exit 4
    fi
  done
  # shellcheck disable=SC2086
  taskset -c $FR_CORE "$FR_BIN" --port $FR_PORT $FR_ARGS >/tmp/azm_fr.log 2>&1 &
  FR_PID=$!
  # shellcheck disable=SC2086
  taskset -c $FR2_CORE "$FR_BIN" --port $FR2_PORT $FR_ARGS >/tmp/azm_fr2.log 2>&1 &
  FR2_PID=$!
  taskset -c $RD_CORE "$REDIS_SERVER" --port $RD_PORT --save '' --appendonly no \
      --daemonize no >/tmp/azm_rd.log 2>&1 &
  RD_PID=$!
  sleep 2
  for p in "$FR_PORT" "$RD_PORT" "$FR2_PORT"; do
    "$REDIS_CLI" -p "$p" ping >/dev/null 2>&1 || { echo "FAIL: port $p not up"; exit 1; }
  done
  if [ -n "$FR_ARGS" ]; then
    for f in /tmp/azm_fr.log /tmp/azm_fr2.log; do
      if grep -qiE "unavailable|ring creation failed|requires a build" "$f"; then
        echo "PREFLIGHT FAIL: FR_ARGS='$FR_ARGS' did not take effect ($f):" >&2
        grep -iE "unavailable|ring creation failed|requires a build" "$f" | head -2 >&2
        echo "  A silently-degraded arm makes the comparison vacuous." >&2
        exit 8
      fi
    done
    echo "fr arms started with: $FR_ARGS"
  fi
  echo "servers: fr=$FR_PID(core$FR_CORE,:$FR_PORT) fr2=$FR2_PID(core$FR2_CORE,:$FR2_PORT) redis=$RD_PID(core$RD_CORE,:$RD_PORT)"
}

# ops_done PORT -> total calls across all commands (INFO commandstats)
ops_done() {
  "$REDIS_CLI" -p "$1" info commandstats 2>/dev/null \
    | grep -oP 'calls=\K[0-9]+' | awk '{s+=$1} END {print s+0}'
}

# measure PORT PID -> "syscalls instr_u instr_k ctxsw taskclock_ms ops"
# A workload token is either a redis-benchmark built-in test name (`set`, `get`,
# …) or `cmd:<words separated by '+'>`, which runs that literal command instead.
# The latter reaches workloads the built-in tests cannot express — above all
# TTL-bearing writes (`cmd:SET+key:__rand_int__+xxx+EX+100`), which are how most
# real Redis deployments actually use the server and which exercise the whole
# volatile-key / active-expire subsystem that a built-in `set` run never touches.
#
# redis-benchmark's grammar is `redis-benchmark [OPTIONS] [COMMAND ARGS...]`, so
# a literal command must come LAST — placing it before the options makes the
# option flags themselves get sent as command arguments and the run silently
# executes nothing.
bench_head() { case "$1" in cmd:*) echo "" ;; *) echo "-t $1" ;; esac; }
bench_tail() { case "$1" in cmd:*) echo "${1#cmd:}" | tr '+' ' ' ;; *) echo "" ;; esac; }

measure() {
  local port="$1" pid="$2" wl="$3" pipe="$4" out
  # `cmd:` workloads legitimately contain glob metacharacters — a stream id is
  # literally `*`, and XADD MAXLEN takes `~`. Unquoted expansion let the shell
  # glob-expand them into filenames, so the benchmark silently ran nothing and
  # the harness reported ops/s = 0 with a garbage per-op divisor. Disable
  # globbing across the expansion; word splitting is still wanted.
  set -f
  # shellcheck disable=SC2046  # deliberate word splitting of the arg builders
  taskset -c "$CLIENT_CORES" "$REDIS_BENCH" -p "$port" $(bench_head "$wl") -n 100000000 \
      -c "$CLIENTS" -P "$pipe" -r "$KEYSPACE" --threads "$CLIENT_THREADS" \
      $(bench_tail "$wl") >/dev/null 2>&1 &
  set +f
  local bpid=$!
  sleep 1                                    # let the connection storm settle
  local ops0; ops0=$(ops_done "$port")
  # The raw_syscalls tracepoint lives in root-only tracefs on this host, so the
  # counter run needs sudo -n. Nothing is mutated and nothing persists; if sudo is
  # unavailable the syscall column degrades to 0 and the u/k split still stands.
  out=$(sudo -n perf stat -x, \
        -e raw_syscalls:sys_enter,instructions:u,instructions:k,context-switches,task-clock \
        -p "$pid" -- sleep "$SECS" 2>&1)
  local ops1; ops1=$(ops_done "$port")
  kill -9 $bpid 2>/dev/null || true; wait $bpid 2>/dev/null || true
  local sc iu ik cs tc
  sc=$(echo "$out" | awk -F, '/raw_syscalls:sys_enter/{print $1}')
  iu=$(echo "$out" | awk -F, '/instructions:u/{print $1}')
  ik=$(echo "$out" | awk -F, '/instructions:k/{print $1}')
  cs=$(echo "$out" | awk -F, '/context-switches/{print $1}')
  tc=$(echo "$out" | awk -F, '/task-clock/{print $1}')
  echo "${sc:-0} ${iu:-0} ${ik:-0} ${cs:-0} ${tc:-0} $((ops1 - ops0))"
}

start_servers
RESULTS=/tmp/azm_syscall_decomp.tsv; : > "$RESULTS"

for WL in ${BENCH_T//,/ }; do
  # warm every engine on THIS workload so no round pays first-touch/rehash cost
  for p in $FR_PORT $RD_PORT $FR2_PORT; do
    set -f
    # shellcheck disable=SC2046
    taskset -c "$CLIENT_CORES" "$REDIS_BENCH" -p $p $(bench_head "$WL") -n 200000 -c 8 -P 16 \
        -r "$KEYSPACE" --threads "$CLIENT_THREADS" $(bench_tail "$WL") >/dev/null 2>&1 || true
    set +f
  done
  # `-P` also accepts a comma list. Sweeping pipeline depth on a fixed workload
  # separates the two costs that a single depth conflates: per-op instructions
  # decompose as  I(P) = C + E/P,  where C is the per-COMMAND cost and E the
  # per-EVENT (per epoll wakeup: read syscall, session swap/snapshot, flush)
  # cost. Fitting across depths recovers C and E for each engine separately,
  # which is what decides whether a residual belongs to the command path or to
  # the event-loop path.
  for P in ${PIPE//,/ }; do
    echo
    echo "### workload=$WL  P=$P  c=$CLIENTS  window=${SECS}s  rounds=$ROUNDS"
    printf '%-6s %-6s %10s %10s %12s %12s %8s %10s %9s\n' \
           round arm 'ops/s' 'sysc/op' 'instr_u/op' 'instr_k/op' 'ctxsw/s' 'ns_cpu/op' 'cpu_util'
    for r in $(seq 1 "$ROUNDS"); do
      # alternate arm order per round so drift cannot alias onto one engine
      if [ $((r % 2)) -eq 1 ]; then ORDER="fr redis fr2"; else ORDER="redis fr fr2"; fi
      for arm in $ORDER; do
        case "$arm" in
          fr)    port=$FR_PORT;  pid=$FR_PID ;;
          fr2)   port=$FR2_PORT; pid=$FR2_PID ;;
          redis) port=$RD_PORT;  pid=$RD_PID ;;
        esac
        read -r sc iu ik cs tc ops <<<"$(measure "$port" "$pid" "$WL" "$P")"
        if [ "${ops:-0}" -le 0 ]; then
          echo "ABORT round $r arm $arm: benchmark executed ZERO operations." >&2
          echo "  A workload that runs nothing yields meaningless per-op divisors." >&2
          echo "  Check the workload token for shell metacharacters (* ~ ?)." >&2
          exit 7
        fi
        # A zeroed counter is a broken measurement, not a fast one. Fail loudly:
        # silently dividing by it prints 0.00 instr/op, which reads like a win.
        if [ -z "${iu:-}" ] || [ "${iu:-0}" = "0" ]; then
          echo "ABORT round $r arm $arm: perf returned no instructions:u — counters unavailable" >&2
          exit 5
        fi
        awk -v wl="$WL/P$P" -v r="$r" -v arm="$arm" -v sc="$sc" -v iu="$iu" -v ik="$ik" \
            -v cs="$cs" -v tc="$tc" -v ops="$ops" -v secs="$SECS" -v out="$RESULTS" 'BEGIN{
          # perf -x, reports task-clock in NANOseconds (raw counter units), not msec.
          opss = ops/secs; nspop = tc/ops; util = 100.0*tc/(secs*1e9);
          printf "%-6s %-6s %10.0f %10.4f %12.1f %12.1f %8.0f %10.1f %8.1f%%\n",
                 r, arm, opss, sc/ops, iu/ops, ik/ops, cs/secs, nspop, util;
          printf "%s\t%s\t%s\t%.0f\t%.6f\t%.2f\t%.2f\t%.1f\t%.2f\t%.1f\n",
                 wl, r, arm, opss, sc/ops, iu/ops, ik/ops, cs/secs, nspop, util >> out;
        }'
      done
    done
  done
done

echo
echo "== medians (statistic = median of per-round values; ratio = median of per-round ratios) =="
python3 - "$RESULTS" <<'PY'
import sys, statistics as st
from collections import defaultdict
wl_rows = defaultdict(lambda: defaultdict(dict))
for line in open(sys.argv[1]):
    wl, r, arm, opss, sc, iu, ik, cs, ns, util = line.split('\t')
    wl_rows[wl][r][arm] = dict(opss=float(opss), sc=float(sc), iu=float(iu),
                               ik=float(ik), cs=float(cs), ns=float(ns),
                               util=float(util))
med_iu = {}
for wl, rows in wl_rows.items():
    def med(arm, k):
        v = [d[arm][k] for d in rows.values() if arm in d]
        return st.median(v) if v else float('nan')
    def ratio(k, a='fr', b='redis'):
        v = [d[a][k]/d[b][k] for d in rows.values() if a in d and b in d and d[b][k]]
        return (st.median(v), min(v), max(v)) if v else (float('nan'),)*3
    print(f'\n--- {wl} ---')
    for k, label in [('opss','ops/s'), ('sc','syscalls/op'), ('iu','instr_u/op'),
                     ('ik','instr_k/op'), ('cs','ctxsw/s'), ('ns','ns_cpu/op')]:
        m, lo, hi = ratio(k)
        n, nlo, nhi = ratio(k, 'fr', 'fr2')
        print(f'{label:>14}  fr={med("fr",k):>12.4f}  redis={med("redis",k):>12.4f}  '
              f'fr/redis={m:.4f} [{lo:.4f},{hi:.4f}]   A/A null fr/fr2={n:.4f} [{nlo:.4f},{nhi:.4f}]')
    med_iu[wl] = {a: med(a, 'iu') for a in ('fr', 'redis', 'fr2')}
    ufr, urd = med('fr', 'util'), med('redis', 'util')
    if ufr == ufr and urd == urd and max(ufr, urd) < 90.0:
        print(f"  !! CLIENT-BOUND: peak server utilisation {max(ufr, urd):.1f}% of a core "
              f"(fr {ufr:.1f}%, redis {urd:.1f}%).")
        print( "     The ops/s ratio above measures the CLIENT and CANNOT show a server-side")
        print( "     effect. Do not quote it. Raise CLIENT_THREADS / CLIENT_CORES and re-run;")
        print( "     instructions:u/op and ns_cpu/op remain valid as per-operation costs.")

# --- I(P) = C + E/P least-squares fit, per (workload, arm) --------------------
# Only runs when a workload was swept over >=3 pipeline depths.
bywl = defaultdict(dict)
for key, d in med_iu.items():
    if '/P' not in key:
        continue
    base, p = key.rsplit('/P', 1)
    bywl[base][int(p)] = d
for base, depths in bywl.items():
    if len(depths) < 3:
        continue
    print(f'\n=== I(P) = C + E/P  fit for {base}  (depths {sorted(depths)}) ===')
    for arm in ('fr', 'redis', 'fr2'):
        xs = [1.0/p for p in sorted(depths)]
        ys = [depths[p][arm] for p in sorted(depths)]
        n = len(xs); sx = sum(xs); sy = sum(ys)
        sxx = sum(x*x for x in xs); sxy = sum(x*y for x, y in zip(xs, ys))
        den = n*sxx - sx*sx
        E = (n*sxy - sx*sy)/den          # slope  = per-EVENT instructions
        C = (sy - E*sx)/n                # intercept = per-COMMAND instructions
        ybar = sy/n
        ss_res = sum((y - (C + E*x))**2 for x, y in zip(xs, ys))
        ss_tot = sum((y - ybar)**2 for y in ys)
        r2 = 1 - ss_res/ss_tot if ss_tot else float('nan')
        print(f'  {arm:>5}:  per-command C = {C:9.1f} instr   '
              f'per-event E = {E:9.1f} instr   R^2 = {r2:.4f}')
PY
