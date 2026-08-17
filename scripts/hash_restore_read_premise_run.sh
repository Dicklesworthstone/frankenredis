#!/bin/bash
# hash_restore_read_premise_run.sh — one-command driver for
# scripts/hash_restore_read_premise_probe.py.
#
# WHY A DRIVER. The probe needs BOTH engines live and their PIDs wired in by hand
# (`--fr-pid`, `--redis-pid`, because it runs `perf stat -p PID`). That friction is
# why its result keeps being forgotten and re-derived: the b1o02 premise-reject it
# produced on 2026-08-08 lives in docs/NEGATIVE_EVIDENCE.md, a DIFFERENT file from
# docs/perf_negative_evidence_ledger.md where the RESTORE rows are written, and
# frankenredis-33832 was filed eight days later restating the isolation framing the
# probe had already refuted. Making it one command is the cheapest fix for that.
#
# THE LAW IT MEASURES: fr decodes eagerly on RESTORE, redis attaches the listpack
# shallowly and walks it on every read. So a RESTORE-in-ISOLATION number flatters
# redis and says nothing about the workload. The deciding quantity is the MARGINAL
# cost of one read on each engine, and from it the break-even reads/RESTORE.
#
#   Do not re-file a RESTORE-isolation gap as a loss. Measure RESTORE+read.
#
# Usage: hash_restore_read_premise_run.sh <fr_binary> [fields] [value_len] [ops] [reps]
set -u

FR_BIN=${1:?usage: hash_restore_read_premise_run.sh <fr_binary> [fields] [value_len] [ops] [reps]}
FIELDS=${2:-64}
VLEN=${3:-16}
OPS=${4:-20000}
REPS=${5:-3}

REPO=/data/projects/frankenredis
REDIS=$REPO/legacy_redis_code/redis/src/redis-server
RS=47711; FR=47712
WORK=$(mktemp -d /data/tmp/premise_run.XXXXXX)

cd "$REPO"
mhz() { awk '/cpu MHz/{s+=$4;n++} END{printf "%.0f", s/n}' /proc/cpuinfo; }
cleanup() { pkill -f "4771[12]" 2>/dev/null; }
trap cleanup EXIT
cleanup; sleep 1

mkdir -p "$WORK/rs" "$WORK/fr"
echo "PRE  loadavg $(cut -d' ' -f1-3 /proc/loadavg)  mean MHz $(mhz)"

setsid "$REDIS" --port $RS --save '' --appendonly no --dir "$WORK/rs" \
       > "$WORK/rs.log" 2>&1 < /dev/null &
setsid "$FR_BIN" --port $FR --save '' --appendonly no --dir "$WORK/fr" \
       > "$WORK/fr.log" 2>&1 < /dev/null &

python3 - "$RS" "$FR" <<'PY'
import socket, sys, time
for port in (int(a) for a in sys.argv[1:3]):
    for _ in range(240):
        try:
            s = socket.create_connection(("127.0.0.1", port), 1)
            s.sendall(b"*1\r\n$4\r\nPING\r\n")
            if s.recv(64):
                s.close(); break
        except OSError:
            time.sleep(0.25)
    else:
        print(f"port {port} never answered PING", file=sys.stderr); sys.exit(1)
print("both engines up")
PY
[ $? -ne 0 ] && exit 2

# Resolve by PORT, not by binary name: redis rewrites its process title to
# `redis-server *:PORT`, so matching the binary misses it (this cost a false RED
# on an unrelated differ earlier in the session).
RS_PID=$(pgrep -f "redis-server \*:$RS" | head -1)
[ -z "$RS_PID" ] && RS_PID=$(pgrep -f "port $RS" | head -1)
FR_PID=$(pgrep -f -- "--port $FR" | head -1)
if [ -z "${RS_PID:-}" ] || [ -z "${FR_PID:-}" ]; then
  echo "REFUSE: could not resolve both PIDs (redis='$RS_PID' fr='$FR_PID')" >&2
  exit 2
fi
echo "pids redis=$RS_PID fr=$FR_PID   fields=$FIELDS value_len=$VLEN ops=$OPS reps=$REPS"

python3 scripts/hash_restore_read_premise_probe.py \
  --fr-port $FR --fr-pid "$FR_PID" --redis-port $RS --redis-pid "$RS_PID" \
  --fields "$FIELDS" --value-len "$VLEN" --ops "$OPS" --reps "$REPS"
rc=$?
echo "POST loadavg $(cut -d' ' -f1-3 /proc/loadavg)  mean MHz $(mhz)"
exit $rc
