#!/bin/bash
# restore_cert_gate.sh — decide whether 33832's wall-clock certification can
# authenticate TODAY, before spending a 9-trial run on it.
#
# WHY. collection_reload_headtohead.py --competitive gates on a two-process A/A
# null in 0.98..1.02. It has now REFUSED THREE TIMES across two agents and three
# placements. 93dc9d374's predicate — "three core blocks under ~50 pct combined
# core+sibling load" — turned out to be NECESSARY but NOT SUFFICIENT: with ONE
# free block it refused at 0.936 and 0.686; with FOUR free blocks, symmetric
# pinning and loadavg 17, it still refused at 1.101 (49f462500).
#
# What that run also showed is the reason for this script. fr_b's own two halves
# — a SINGLE process, on a block measured at 0 pct contention — came out 0.804034x.
# A 20 pct drift against itself cannot be process placement, because there is one
# process and it never moved. So the term that actually fails is same-process
# drift over time, and the free-block sweep cannot see it.
#
# THIS GATE SAMPLES THAT TERM DIRECTLY, with a SHORT probe instead of a 9-trial
# certification. One short run tells you whether the long one can pass.
#
# WARM-UP IS NOT OPTIONAL HERE (230c674ec): the within-process drift is a settling
# transient spanning ~10 trials, so an unwarmed probe measures the transient rather
# than the engine. Every probe below discards WARMUP passes per arm (default 8, the
# measured count). An earlier version of this gate did not, and concluded from six
# unwarmed samples that the host could not be certified at all -- that conclusion is
# retracted.
#
#   stage 1  free-block sweep      (necessary, cheap, catches the busy-fleet case)
#   stage 2  one-process null probe (the term that actually fails)
#
# Exit 0 = GO, both stages clear, spend the certification.
# Exit 1 = the one-process null is out of band; a 9-trial run would refuse too.
# Exit 2 = fewer than three free blocks, or below the 42G disk stop.
#
# Usage: restore_cert_gate.sh <fr_binary> [trials] [samples] [warmup_passes]
set -u

FR=${1:?usage: restore_cert_gate.sh <fr_binary> [trials] [samples] [warmup]}
TRIALS=${2:-3}
SAMPLES=${3:-3}
WARMUP=${4:-8}
REPO=/data/projects/frankenredis
REDIS=$REPO/legacy_redis_code/redis/src/redis-server
RS=47621; FA=47622; FB=47623
WORK=$(mktemp -d /data/tmp/restore_cert_gate.XXXXXX)

cd "$REPO"
mhz() { awk '/cpu MHz/{s+=$4;n++} END{printf "%.0f", s/n}' /proc/cpuinfo; }
cleanup() { pkill -f "4762[123]" 2>/dev/null; }
trap cleanup EXIT

free_g=$(df -BG --output=avail /data | tail -1 | tr -dc '0-9')
if [ "$free_g" -lt 42 ]; then
  echo "REFUSE: /data has ${free_g}G free, below the 42G hard stop" >&2
  exit 2
fi

echo "== stage 1: free-block sweep (necessary, not sufficient) =="
echo "   loadavg $(cut -d' ' -f1-3 /proc/loadavg)   mean MHz $(mhz)   /data ${free_g}G"
BLOCKS=$(ps -eo psr,pcpu --no-headers | awk '{l[$1]+=$2} END {for(c=0;c<64;c++) printf "%d %.0f\n", c, l[c]}' \
  | awk '{ core=$1%32; s[core]+=$2 }
         END { for(b=0;b<8;b++){ t=0; for(c=b*4;c<b*4+4;c++) t+=s[c]; if(t<50) printf "%d ", b } }')
NFREE=$(echo $BLOCKS | wc -w)
echo "   free blocks: [${BLOCKS}] -> $NFREE (need 3)"
if [ "$NFREE" -lt 3 ]; then
  echo "REFUSE: only $NFREE uncontended blocks; the fleet is busy. Do not spend the run." >&2
  echo "   Use scripts/restore_instr_per_op.py instead -- load-immune, no pinning." >&2
  exit 2
fi

set -- $BLOCKS
B_REDIS=$1; B_A=$2; B_B=$3
c_of() { echo "$(( $1 * 4 ))-$(( $1 * 4 + 3 ))"; }
echo "   pinning redis->$(c_of $B_REDIS)  fr_a->$(c_of $B_A)  fr_b->$(c_of $B_B)"

mkdir -p "$WORK/rs" "$WORK/fa" "$WORK/fb"
setsid taskset -c "$(c_of $B_REDIS)" "$REDIS" --port $RS --save '' --appendonly no \
       --dir "$WORK/rs" --enable-debug-command yes > "$WORK/rs.log" 2>&1 < /dev/null &
setsid taskset -c "$(c_of $B_A)" "$FR" --port $FA --save '' --appendonly no \
       --dir "$WORK/fa" --enable-debug-command yes > "$WORK/fa.log" 2>&1 < /dev/null &
setsid taskset -c "$(c_of $B_B)" "$FR" --port $FB --save '' --appendonly no \
       --dir "$WORK/fb" --enable-debug-command yes > "$WORK/fb.log" 2>&1 < /dev/null &

python3 - "$RS" "$FA" "$FB" <<'PY'
import socket, sys, time
for port in (int(a) for a in sys.argv[1:4]):
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
PY
if [ $? -ne 0 ]; then echo "REFUSE: an arm never came up" >&2; exit 2; fi

echo "== stage 2: one-process null, $SAMPLES probes of $TRIALS trials =="
# THREE PROBES, NOT ONE, and this is a correction to this script's first version.
# Measured across three separate UNWARMED probes at FALLING load, the one-process
# null read 0.933081x (loadavg 20.0), 0.986818x (16.0) and 1.054018x (13.3) -- a
# 6 pct swing with no relationship to load. Those readings are superseded as
# EVIDENCE ABOUT THE HOST (they were inside the settling transient), but they still
# justify sampling more than once: a single draw decided GO or REFUSE by luck. A single probe therefore decides GO or REFUSE partly
# by luck, which is exactly the failure this gate exists to catch, reproduced one
# level up. The median of three is not a cure, but it stops one draw deciding, and
# printing every sample lets the reader see the spread rather than trust the middle.
#
# This is NOT retry-until-pass: every sample is reported, the decision uses the
# MEDIAN, and a run where the samples disagree is called out rather than rerun.
SAMPLES_SEEN=""
LAST_OUT=""
for probe in $(seq 1 "$SAMPLES"); do
  OUT=$(timeout 900 python3 scripts/collection_reload_headtohead.py $RS $FA \
          --competitive --fr-aa-port $FB --trials "$TRIALS" \
          --warmup-passes "$WARMUP" 2>&1)
  LAST_OUT="$OUT"
  s=$(echo "$OUT" | sed -n 's/.*fr_b halves, one process) median=\([0-9.]*\)x.*/\1/p')
  aa=$(echo "$OUT" | sed -n 's/.*fr_a\/fr_b, two processes) median=\([0-9.]*\)x.*/\1/p')
  [ -z "$s" ] && { echo "REFUSE: probe $probe reported no one-process null" >&2; exit 1; }
  printf "   probe %s: one-process %sx   two-process %sx   loadavg %s   MHz %s\n" \
         "$probe" "$s" "${aa:-?}" "$(cut -d' ' -f1 /proc/loadavg)" "$(mhz)"
  SAMPLES_SEEN="$SAMPLES_SEEN $s"
done
OUT="$LAST_OUT"

SAME=$(echo $SAMPLES_SEEN | tr ' ' '\n' | sort -g | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')
SPREAD=$(echo $SAMPLES_SEEN | tr ' ' '\n' | sort -g \
         | awk '{a[NR]=$1} END{printf "%.4f", a[NR]-a[1]}')
echo "   samples:${SAMPLES_SEEN}  median ${SAME}x  spread ${SPREAD}"
if awk -v s="$SPREAD" 'BEGIN{exit !(s>0.04)}'; then
  echo "REFUSE: the one-process null itself spreads ${SPREAD} across probes, wider than" >&2
  echo "        the 0.04 band it is being judged against. The median is not meaningful" >&2
  echo "        and a 9-trial run cannot be predicted from it." >&2
  echo >&2
  # CORRECTED (230c674ec). This block used to report six samples spanning
  # 0.917-1.056 and conclude the spread was a property of the host that re-running
  # would not fix. That was wrong, and the samples were the evidence for it: ALL SIX
  # were taken WITHOUT warm-up, and the drift is a SETTLING TRANSIENT spanning about
  # ten trials, measured by --drift-curve (first quartile fastest, then a plateau; a
  # monotone-rise fraction near 0.5 rules out steady growth). Warming 8 passes moved
  # the same-process null from 0.771 to 1.047 and the two-process null from 0.931 to
  # 0.974. This gate now warms by default, so a wide spread here is NOT the same
  # observation my earlier samples were.
  echo "        NOTE: the pre-warmup samples this gate used to cite (0.917-1.056) are" >&2
  echo "        SUPERSEDED -- that spread was a ~10-trial settling transient, not a" >&2
  echo "        host property (230c674ec). This run warmed $WARMUP passes per arm." >&2
  echo "        A wide spread STILL here is worth re-running in a quieter window:" >&2
  echo "        the warmed null reached 0.974 at loadavg 32, and whether it enters" >&2
  echo "        0.98..1.02 at loadavg ~15 is measured, not settled." >&2
  echo "        Meanwhile the load-immune instruments answer the narrower question:" >&2
  echo "          scripts/restore_instr_per_op.py     fr/redis instr/op ratio" >&2
  echo "          scripts/restore_profile_frames.py   where the instructions go" >&2
  exit 1
fi
echo "   one-process null = ${SAME}x  (band 0.98..1.02)"
if awk -v v="$SAME" 'BEGIN{exit !(v>=0.98 && v<=1.02)}'; then
  echo "GO: same-process drift is in band. A 9-trial certification can authenticate."
  exit 0
fi

# The dangerous case, seen on this gate's FIRST live run: the harness ACCEPTED
# ("COMPETITIVE ROW — A/A median accepted") on a two-process A/A of 1.006297x whose
# CI was [0.870406, 1.122179] — a 25 pct span — while this same-process null read
# 0.933081x. A point estimate can land in 0.98..1.02 by luck when the underlying
# term is that noisy, and the CI is what gives it away. Say so loudly, because a row
# banked from that state looks authenticated and is not.
if echo "$OUT" | grep -q "COMPETITIVE ROW"; then
  echo
  echo "!! THE HARNESS ACCEPTED AND THIS GATE DOES NOT. Its two-process A/A point" >&2
  echo "   estimate landed in band, but the same process drifts ${SAME}x against" >&2
  echo "   ITSELF, so that pass is luck rather than stability -- check the A/A CI" >&2
  echo "   printed above, which is wide when this happens. Do NOT bank the row." >&2
fi
echo "REFUSE: one-process null ${SAME}x is out of band -- a single process drifts against" >&2
echo "        ITSELF by that much, so the two-process A/A cannot land in 0.98..1.02 and a" >&2
echo "        9-trial run would refuse. This is NOT a placement problem; do not re-pin." >&2
echo "        Record the reading and use scripts/restore_instr_per_op.py instead." >&2
exit 1
