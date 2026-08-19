#!/usr/bin/env bash
# bitcmd_instr.sh -- instr/op for the commands qxdyn touched, fr vs Redis 7.2.4.
#
# WHY INSTRUCTIONS, NOT WALL CLOCK: this host is at loadavg ~18 with other
# tenants building. Retired user-space instructions for a FIXED job do not care
# what a neighbour is doing; wall clock does. Same instrument the repo's
# instr_per_op_vs_redis.sh uses, and for the same stated reason.
#
# WHAT IS MEASURED: SETBIT / SETRANGE / BITFIELD SET are exactly the paths that
# gained a `!must_obey_client` conjunct and, for BITFIELD, a hoisted per-command
# `offset_limit`. PING is the control -- untouched by this work, so it sizes
# any drift that is not about these commands. The A/A null re-measures the SAME
# fr binary twice so a ratio can be read against its own noise floor.
set -euo pipefail

ROOT=/data/projects/frankenredis
FR_BIN=$ROOT/target/release/frankenredis
REDIS=$ROOT/legacy_redis_code/redis/src/redis-server
BENCH=$ROOT/legacy_redis_code/redis/src/redis-benchmark
CLI=$ROOT/legacy_redis_code/redis/src/redis-cli
FR_PORT=27881; RD_PORT=27882
N=${N:-200000}; C=${C:-32}; DRAWS=${DRAWS:-3}
SERVER_CPUS=${SERVER_CPUS:-0-15}; CLIENT_CPUS=${CLIENT_CPUS:-16-31}

for f in "$FR_BIN" "$REDIS" "$BENCH" "$CLI"; do
  [ -x "$f" ] || { echo "FAIL: missing $f" >&2; exit 3; }
done
for p in $FR_PORT $RD_PORT; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "FAIL: port $p bound" >&2; exit 5; }
done

echo "== identity =="
echo "  fr    ELF $(sha256sum "$FR_BIN"  | cut -c1-16)   $(stat -c%y "$FR_BIN"  | cut -d. -f1)"
echo "  redis ELF $(sha256sum "$REDIS"   | cut -c1-16)"
echo "  job   n=$N c=$C draws=$DRAWS  server cpus=$SERVER_CPUS client cpus=$CLIENT_CPUS"
echo

PIDS=()
cleanup(){ for p in "${PIDS[@]:-}"; do [ -n "${p:-}" ] && kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

taskset -c "$SERVER_CPUS" "$FR_BIN" --port $FR_PORT >/tmp/bc_fr.log 2>&1 &
FR_PID=$!; PIDS+=($FR_PID)
taskset -c "$SERVER_CPUS" "$REDIS" --port $RD_PORT --save '' --appendonly no --io-threads 1 \
  >/tmp/bc_rd.log 2>&1 &
RD_PID=$!; PIDS+=($RD_PID)
sleep 2
for p in $FR_PORT $RD_PORT; do
  "$CLI" -p $p ping >/dev/null 2>&1 || { echo "FAIL: no PONG on $p" >&2; exit 6; }
done
echo "  fr    RUNNING-IMAGE $(sha256sum /proc/$FR_PID/exe | cut -c1-16)"
echo "  redis RUNNING-IMAGE $(sha256sum /proc/$RD_PID/exe | cut -c1-16)"
echo

mhz(){ awk '/cpu MHz/{s+=$4;n++} END{if(n)printf "%.0f",s/n}' /proc/cpuinfo; }
la(){ cut -d' ' -f1-3 /proc/loadavg; }

# instr/op for ONE arm over ONE fixed job, attributed to the server pid so the
# client's own work stays out of the count.
run(){ # $1=pid $2=port $3.. = command words
  local pid=$1 port=$2; shift 2
  local out; out=$(mktemp)
  perf stat -e instructions:u -p "$pid" -x, -o "$out" -- \
    taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -n "$N" -c "$C" -P 1 -q "$@" \
    >/dev/null 2>&1 || true
  local i; i=$(awk -F, '/instructions/{print $1}' "$out")
  rm -f "$out"
  awk -v i="${i:-0}" -v n="$N" 'BEGIN{printf "%.1f", i/n}'
}

printf "%-12s %-6s %12s %12s %9s   %-22s %s\n" FAMILY DRAW "fr instr/op" "rd instr/op" "ratio" "loadavg(fr|rd)" "MHz(fr|rd)"
for fam in "setbit:SETBIT bk 100 1" \
           "setrange:SETRANGE sk 100 xxxxxxxx" \
           "bitfield:BITFIELD fk SET u8 100 7" \
           "ping:PING"; do
  name=${fam%%:*}; cmd=${fam#*:}
  for d in $(seq 1 "$DRAWS"); do
    la_fr=$(la); mhz_fr=$(mhz)
    fr=$(run $FR_PID $FR_PORT $cmd)
    la_rd=$(la); mhz_rd=$(mhz)
    rd=$(run $RD_PID $RD_PORT $cmd)
    printf "%-12s %-6s %12s %12s %9s   %-22s %s\n" "$name" "$d" "$fr" "$rd" \
      "$(awk -v a="$fr" -v b="$rd" 'BEGIN{if(b>0)printf "%.4f",a/b; else print "n/a"}')" \
      "$la_fr | $la_rd" "$mhz_fr | $mhz_rd"
  done
done

echo
echo "== A/A NULL: the SAME fr binary measured twice, same job =="
for d in 1 2 3; do
  a=$(run $FR_PID $FR_PORT SETBIT bk 100 1)
  b=$(run $FR_PID $FR_PORT SETBIT bk 100 1)
  printf "  null draw %s: %s vs %s -> %s\n" "$d" "$a" "$b" \
    "$(awk -v x="$a" -v y="$b" 'BEGIN{if(y>0)printf "%.4f",x/y; else print "n/a"}')"
done
