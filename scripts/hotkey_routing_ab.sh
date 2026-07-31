#!/usr/bin/env bash
# Paired A/B: connection PLACEMENT policy on the hot-key fixture.
#
#   arm K = keyed   : shared-tree binary, connections placed by hashing FIRST key
#   arm R = roundrobin : my patch, connections placed round-robin at accept
#
# Both arms are the SAME design otherwise (partitioned keyspace, same spin lock),
# run one at a time on the SAME cpuset, with arm order alternating per round so a
# warming or drifting host cannot favour one. The discriminator is the per-thread
# CPU census -- a count, not a stopwatch -- taken while the job runs.
set -euo pipefail

ROOT=/data/projects/frankenredis
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
K_BIN=/data/tmp/claude-1000/-data-projects-frankenredis/258fb22f-5d58-4d03-a462-8546093e21cb/scratchpad/keyedtarget/release/frankenredis
R_BIN="$ROOT/target/release/frankenredis"
SERVER_CPUS="0-15,32-47"
CLIENT_CPUS="16-31,48-63"
WORKERS=16
N="${N:-300000}"
CONNS="${CONNS:-64}"
ROUNDS="${ROUNDS:-3}"
PORT_BASE=27900

census() {  # $1=pid  -> "active_of_total totalcpu"
  python3 - "$1" <<'PY'
import os,sys,time
pid=sys.argv[1]
def snap():
    out={}
    for t in os.listdir(f"/proc/{pid}/task"):
        try:
            f=open(f"/proc/{pid}/task/{t}/stat").read()
            name=f[f.index('(')+1:f.rindex(')')]
            r=f[f.rindex(')')+2:].split()
            out[t]=(name,int(r[11])+int(r[12]))
        except Exception: pass
    return out
a=snap(); time.sleep(3.0); b=snap()
HZ=os.sysconf('SC_CLK_TCK')
rows=[(100.0*(b[t][1]-a[t][1])/HZ/3.0) for t in b if t in a and 'set-get' in b[t][0]]
act=[x for x in rows if x>5.0]
print(f"{len(act)}/{len(rows)} {sum(rows):.0f}")
PY
}

run_arm() { # $1=bin $2=label $3=port -> "rps active total"
  local bin="$1" label="$2" port="$3" pid rps c
  taskset -c "$SERVER_CPUS" "$bin" --port "$port" \
      --experimental-sharded-set-get-workers "$WORKERS" >/tmp/ab_$label.log 2>&1 &
  pid=$!
  sleep 2
  taskset -c "$CLIENT_CPUS" "$BENCH" -p "$port" -t lpush,lpop,hset -n "$N" \
      -c "$CONNS" -P 1 --threads 8 --csv >/tmp/ab_$label.csv 2>/dev/null &
  local bp=$!
  sleep 1
  c=$(census "$pid")
  wait $bp 2>/dev/null || true
  rps=$(awk -F, 'NR>1{gsub(/"/,"",$2); s+=$2; n++} END{if(n)printf "%.0f", s/n}' /tmp/ab_$label.csv)
  kill -9 "$pid" 2>/dev/null || true
  wait 2>/dev/null || true
  echo "$rps $c"
}

echo "== hot-key connection-placement A/B =="
echo "  fixture   -t lpush,lpop,hset (redis-benchmark points all three at ONE key)"
echo "  n=$N per family, c=$CONNS, P=1, $WORKERS reactors, cpuset $SERVER_CPUS"
echo "  K bin sha $(sha256sum "$K_BIN" | cut -c1-16)   R bin sha $(sha256sum "$R_BIN" | cut -c1-16)"
echo

# live redis reference, once
taskset -c "$SERVER_CPUS" "$REDIS" --port $((PORT_BASE+9)) --save '' --appendonly no \
    --io-threads 8 --io-threads-do-reads yes >/tmp/ab_redis.log 2>&1 &
RPID=$!; sleep 2
taskset -c "$CLIENT_CPUS" "$BENCH" -p $((PORT_BASE+9)) -t lpush,lpop,hset -n "$N" \
    -c "$CONNS" -P 1 --threads 8 --csv >/tmp/ab_redis.csv 2>/dev/null
REDIS_RPS=$(awk -F, 'NR>1{gsub(/"/,"",$2); s+=$2; n++} END{if(n)printf "%.0f", s/n}' /tmp/ab_redis.csv)
kill -9 $RPID 2>/dev/null || true
echo "  live redis-server mean rps over the 3 families: $REDIS_RPS"
echo
printf '%-6s %12s %10s %10s %12s %10s %10s\n' round 'keyed rps' 'reactors' 'srvCPU' 'roundrobin' 'reactors' 'srvCPU'
for r in $(seq 1 "$ROUNDS"); do
  p=$((PORT_BASE + r*2))
  if [ $((r % 2)) -eq 1 ]; then
    read -r krps kact kcpu <<<"$(run_arm "$K_BIN" keyed $p)"
    read -r rrps ract rcpu <<<"$(run_arm "$R_BIN" rr $((p+1)))"
  else
    read -r rrps ract rcpu <<<"$(run_arm "$R_BIN" rr $((p+1)))"
    read -r krps kact kcpu <<<"$(run_arm "$K_BIN" keyed $p)"
  fi
  printf '%-6s %12s %10s %9s%% %12s %10s %9s%%\n' "$r" "$krps" "$kact" "$kcpu" "$rrps" "$ract" "$rcpu"
done
echo
echo "  reactors column = threads above 5% CPU / total reactor threads"
