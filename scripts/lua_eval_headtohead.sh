#!/usr/bin/env bash
# lua_eval_headtohead.sh — EVAL competitive harness with a same-invocation A/A null.
#
# WHY THIS EXISTS. The Lua interpreter has an excellent MICRObench
# (crates/fr-command/benches/lua_rediscall_loop.rs, instructions:u, CV ~0.00006%),
# and every lever on frankenredis-lua-rediscall-loop-interpreter-bound-d3al0 has
# been measured on it. That bench compares fr to fr: it is a SELF-SPEEDUP
# instrument, which is maintenance, not campaign output. It cannot say whether
# any of it moved fr relative to the incumbent, and the ledger carries an
# inherited verdict that says it cannot --
#
#   docs/perf_negative_evidence_ledger.md:7676 (2026-06-28 AmberRiver)
#   "EVAL is conclusively a STRUCTURAL-only gap (bytecode VM / mlua-LuaJIT)."
#
# That row swapped the Lua maps' HASH FUNCTION (SipHash -> foldhash), measured
# 1.00-1.02x at e2e on a host at load ~12, and concluded hashing is not the EVAL
# bottleneck. It is a fair row, but it is not a measurement of where EVAL stands
# after the interpreter shed 23.6% of its per-call instructions in one day. This
# harness is the instrument that can answer that, so the verdict can be re-tested
# with evidence rather than inherited.
#
# WHAT IT MEASURES, all arms in ONE invocation, all driven by the VENDORED
# redis-benchmark so neither engine gets a bespoke client:
#
#   null   = fr_A2 / fr_A     two instances of the SAME binary. This is the A/A
#                             control. It absorbs core identity, start order and
#                             client placement -- the things that bias an A/B by
#                             10-14% on this host -- and any effect smaller than
#                             the null's spread is not a result.
#   effect = fr_B  / fr_A     the lever, when FR_BIN_B is supplied.
#   compA  = fr_A  / redis    where fr actually stands. THIS is the campaign number.
#   compB  = fr_B  / redis    where the lever puts it.
#
# Usage:
#   FR_BIN_A=/tmp/fr_base [FR_BIN_B=/tmp/fr_cand] scripts/lua_eval_headtohead.sh
#     -n <requests>  -c <clients>  -R <rounds>  --calls <redis.calls per eval>
#
# Exit 0 = measured · 4 = port busy · 5 = engine disagreement (see below) · 6 = no quiet cores
set -euo pipefail

REQUESTS=20000; CLIENTS=8; ROUNDS=5; CALLS=50; PIPELINE=1
FR_BIN_A="${FR_BIN_A:-/tmp/fr_eval_a}"
FR_BIN_B="${FR_BIN_B:-}"
# A few idle CPUs do not make a loaded host usable: migrating background work
# corrupts the A/A control even when the selected CPUs looked idle at selection.
# That reasoning is right and the gate stays fail-closed. What was wrong was its
# UNIT.
#
# The ceiling was an ABSOLUTE one-minute loadavg of 1.0, justified as "historical
# admissible rows were taken below load 1.0". Measured, that premise does not
# hold: a 21-run batch on 2026-08-14 (PearlPuma) taken at loadavg 16.53 on this
# 64-core host produced an A/A null median of 1.0031 with an across-run 95% CI of
# [0.9683, 1.0292] -- containing 1.0, widest bound 3.17%, n=13 -- and a
# COMPETITIVE fr/redis of 0.8977 whose CI excluded 1.0. The control was not
# corrupted at sixteen times the ceiling, so the ceiling is not calibrated to the
# thing it claims to protect.
#
# The reason is that loadavg counts the WHOLE machine while this harness pins
# every arm to a core whose SMT sibling is also verified idle (pick_cores below,
# which is the load-bearing check). On a 64-core box shared by an agent fleet,
# loadavg 8 is 13% utilisation with 35 quiet cores, and an absolute ceiling of
# 1.0 is unreachable -- it vetoed 21 of 21 runs, i.e. it did not make the number
# safer, it made the measurement impossible.
#
# So the ceiling is now expressed PER CORE and still fails closed. The default
# 0.30 is set from the demonstrated point, not from taste: an admissible null was
# measured at 0.258 load per core, and 0.30 leaves a small margin above it. On a
# single-core host this is STRICTER than the old 1.0.
#
# INTEGRITY CHECK, per the gate-change rule in /data/projects/AGENTS.md: this
# change makes exactly one previously-vetoed row decidable, and that row LOSES
# (fr/redis 0.8977x on the EVAL 50x GET loop). A gate change that suddenly
# produces wins is a loosening; this one admits a loss.
#
# MAX_LOAD_1 is still honoured when set explicitly, as an absolute override for
# harness diagnosis -- never as a campaign verdict.
MAX_LOAD_PER_CORE="${MAX_LOAD_PER_CORE:-0.30}"
while [ $# -gt 0 ]; do
  case "$1" in
    -n) REQUESTS="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    # (d3al0) Pipelining depth. Added because at -P 1 an interpreter lever cannot
    # clear this harness's own A/A null: a 10.516% instruction reduction measured
    # +1.34% e2e with a CI containing 1.0, since most of an unpipelined round trip
    # is syscall and scheduling, not interpretation. Amortising the round trip
    # over a batch raises the interpreter's SHARE of the measured work, which is
    # the only way a lever of that size becomes resolvable at the command level.
    -P) PIPELINE="$2"; shift 2;;
    --calls) CALLS="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
FR_A_PORT=27571; FR_A2_PORT=27572; RD_PORT=27573; FR_B_PORT=27574

LOAD_1=$(awk '{print $1}' /proc/loadavg)
CORES_TOTAL=$(nproc)
# An explicit MAX_LOAD_1 keeps its absolute meaning; otherwise the ceiling scales
# with the core count, because that is what "is this host busy" actually means on
# a machine whose cores are individually preflighted below.
if [ -n "${MAX_LOAD_1:-}" ]; then
  MAX_LOAD_EFFECTIVE="$MAX_LOAD_1"
  MAX_LOAD_RULE="absolute MAX_LOAD_1 override"
else
  MAX_LOAD_EFFECTIVE=$(awk -v per="$MAX_LOAD_PER_CORE" -v n="$CORES_TOTAL" \
    'BEGIN { printf "%.2f", per * n }')
  MAX_LOAD_RULE="${MAX_LOAD_PER_CORE}/core x ${CORES_TOTAL} cores"
fi
if ! awk -v one_min_load="$LOAD_1" -v max="$MAX_LOAD_EFFECTIVE" 'BEGIN { exit !(one_min_load <= max) }'; then
  echo "PREFLIGHT FAIL: one-minute host load $LOAD_1 exceeds $MAX_LOAD_EFFECTIVE" \
       "($MAX_LOAD_RULE); wait for a quiet host" >&2
  exit 6
fi

# The script under test is the microbench's script verbatim, so the harness and
# the instructions:u bench exercise the SAME interpreter path.
SCRIPT="for i=1,${CALLS} do redis.call('GET', KEYS[1]) end return 1"

nproc_n=$(nproc)
# Pick cores whose SMT sibling is ALSO idle: a busy sibling costs 10-14% and
# would land entirely on whichever arm drew that core.
pick_cores() {
  local want="$1" out=() c sib u v
  # Skip core 0: it absorbs most device interrupts, so whichever arm draws it is
  # systematically penalised. Measured here before this guard existed -- with the
  # arms fixed to cores 0/1/3 the A/A null sat at 1.046 with a 10.8% spread, and
  # lengthening the runs did not shrink it because the bias is positional, not
  # statistical.
  for c in $(seq 1 $((nproc_n - 1))); do
    sib=$(( c >= nproc_n/2 ? c - nproc_n/2 : c + nproc_n/2 ))
    u=$(ps -eo psr,pcpu --no-headers | awk -v k="$c"   '$1==k {t+=$2} END {printf "%.0f", t+0}')
    v=$(ps -eo psr,pcpu --no-headers | awk -v k="$sib" '$1==k {t+=$2} END {printf "%.0f", t+0}')
    if [ "$u" -lt 2 ] && [ "$v" -lt 2 ]; then
      out+=("$c"); [ "${#out[@]}" -ge "$want" ] && break
    fi
  done
  [ "${#out[@]}" -lt "$want" ] && return 1
  ( IFS=,; echo "${out[*]}" )
}
NEED=4; [ -n "$FR_BIN_B" ] && NEED=5
ALL=$(pick_cores "$NEED") || { echo "PREFLIGHT FAIL: fewer than $NEED quiet cores; wait for a window" >&2; exit 6; }
if [ -n "$FR_BIN_B" ]; then
  IFS=, read -r FR_A_CORE FR_A2_CORE RD_CORE FR_B_CORE CLIENT_CORE <<<"$ALL"
else
  IFS=, read -r FR_A_CORE FR_A2_CORE RD_CORE CLIENT_CORE <<<"$ALL"; FR_B_CORE=""
fi

# The A/A null is only a null if both arms are the SAME BYTES.
FR_BIN_A2=/tmp/fr_eval_a2_null
cp "$FR_BIN_A" "$FR_BIN_A2"
cmp -s "$FR_BIN_A" "$FR_BIN_A2" || { echo "FAIL: null arm is not byte-identical" >&2; exit 5; }

echo "== binaries (self-reported by sha256sum of the image actually exec'd) =="
echo "fr_A   $(sha256sum "$FR_BIN_A")"
echo "fr_A2  $(sha256sum "$FR_BIN_A2")   <- A/A null arm, must equal fr_A"
[ -n "$FR_BIN_B" ] && echo "fr_B   $(sha256sum "$FR_BIN_B")"
if [ -n "$FR_BIN_B" ] && cmp -s "$FR_BIN_A" "$FR_BIN_B"; then
  echo "FAIL: candidate is byte-identical to baseline -- nothing to measure" >&2; exit 5
fi
echo "redis  $(sha256sum "$REDIS")"
"$REDIS" --version | head -1
echo "host $(hostname) load $(cut -d' ' -f1-3 /proc/loadavg)  nproc $nproc_n" \
     "(load ceiling $MAX_LOAD_EFFECTIVE, $MAX_LOAD_RULE)"
echo "cores: fr_A=$FR_A_CORE fr_A2=$FR_A2_CORE redis=$RD_CORE fr_B=${FR_B_CORE:-none} client=$CLIENT_CORE"
echo "workload: EVAL with $CALLS redis.call('GET') per eval, n=$REQUESTS c=$CLIENTS P=$PIPELINE rounds=$ROUNDS"

PORTS="$FR_A_PORT $FR_A2_PORT $RD_PORT"; [ -n "$FR_BIN_B" ] && PORTS="$PORTS $FR_B_PORT"
for p in $PORTS; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 4; }
done

PIDS=""
cleanup() { for p in $PIDS; do kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT
taskset -c "$FR_A_CORE"  "$FR_BIN_A"  --port $FR_A_PORT  >/tmp/lehh_fra.log  2>&1 & PIDS="$PIDS $!"
taskset -c "$FR_A2_CORE" "$FR_BIN_A2" --port $FR_A2_PORT >/tmp/lehh_fra2.log 2>&1 & PIDS="$PIDS $!"
# --io-threads is left at 1: redis's io-threads busy-wait fakes per-op cost.
taskset -c "$RD_CORE" "$REDIS" --port $RD_PORT --save '' --appendonly no >/tmp/lehh_rd.log 2>&1 & PIDS="$PIDS $!"
if [ -n "$FR_BIN_B" ]; then
  taskset -c "$FR_B_CORE" "$FR_BIN_B" --port $FR_B_PORT >/tmp/lehh_frb.log 2>&1 & PIDS="$PIDS $!"
fi
sleep 2

# SELF-REPORTED RUNNING-IMAGE HASHES. Hashing the file we intended to launch
# proves only what was on disk; hashing /proc/<pid>/exe hashes the image the
# kernel actually mapped for the process that is about to be benchmarked, which
# is the thing a ledger entry is claiming about.
echo "== self-reported running-image SHA-256 (sha256sum of /proc/<pid>/exe) =="
report_running_image() {  # PID LABEL
  local sha
  sha=$(sudo -n sha256sum "/proc/$1/exe" 2>/dev/null | awk '{print $1}')
  [ -z "$sha" ] && sha=$(sha256sum "/proc/$1/exe" 2>/dev/null | awk '{print $1}')
  echo "  $2 benchmarked server ELF self-reported SHA-256 ${sha:-UNAVAILABLE} (pid $1)"
}
set -- $PIDS
report_running_image "$1" fr_A; report_running_image "$2" fr_A2; report_running_image "$3" redis
[ -n "$FR_BIN_B" ] && report_running_image "$4" fr_B

# SEED, then PROVE BOTH ENGINES ACTUALLY RUN THE SCRIPT.
# redis-benchmark counts an ERROR REPLY as a completed request, so an engine that
# refuses the script benchmarks beautifully. Every arm must return the integer 1
# for the exact script the benchmark will send, or the run is void.
for p in $PORTS; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "FAIL: port $p not up"; exit 1; }
  "$CLI" -p "$p" set k val >/dev/null
  got=$("$CLI" -p "$p" eval "$SCRIPT" 1 k 2>&1)
  if [ "$got" != "1" ]; then
    echo "FAIL: port $p did not execute the script (got: $got)" >&2
    echo "  a non-integer reply here means the ratio below would be measuring error replies" >&2
    exit 5
  fi
  echo "  port $p: EVAL returns $got, GET k = $("$CLI" -p "$p" get k)"
done

RES=/tmp/lehh.tsv; : > "$RES"
run_arm() {  # PORT TAG ROUND
  # NOT --csv: redis-benchmark names the test after the command it sent, and this
  # script contains both commas and double quotes, so the "CSV" it emits is
  # unparseable by field position (the rps lands in a different column depending
  # on how many commas the script has). The -q line ends in an unambiguous
  # "<rps> requests per second" regardless of the script text.
  local rps
  rps=$(taskset -c "$CLIENT_CORE" "$BENCH" -p "$1" -n "$REQUESTS" -c "$CLIENTS" -P "$PIPELINE" \
          -q eval "$SCRIPT" 1 k 2>/dev/null \
        | tr '\r' '\n' | grep -oE '[0-9]+\.[0-9]+ requests per second' \
        | tail -1 | awk '{print $1}')
  [ -z "$rps" ] && { echo "FAIL: no rps parsed for $2 (port $1)" >&2; exit 5; }
  echo -e "$3\t$2\t$rps" >> "$RES"
}

# Core assignment is rotated every round (see below), so collect the server
# cores and pids as parallel lists in a fixed arm order.
ARM_PIDS=(); ARM_CORES=()
read -r -a ARM_PIDS <<<"$(echo $PIDS)"
if [ -n "$FR_BIN_B" ]; then
  ARM_CORES=("$FR_A_CORE" "$FR_A2_CORE" "$RD_CORE" "$FR_B_CORE")
else
  ARM_CORES=("$FR_A_CORE" "$FR_A2_CORE" "$RD_CORE")
fi

# ROTATE WHICH PHYSICAL CORE EACH ARM RUNS ON, every round. Without this the
# comparison silently measures core identity: cores differ in interrupt load and
# cache/NUMA placement by 10-14% on this class of host, and a fixed assignment
# hands that difference to one arm permanently. Rotating makes every arm spend
# the same number of rounds on every core, so the bias cancels in the median
# instead of accumulating -- which is what turns the A/A null from a systematic
# 1.046 into an actual control.
rotate_cores() {  # CYCLE_INDEX (NOT the round number -- see below)
  # DO NOT pass the round number here. Core rotation and ORDER rotation were both
  # keyed on `r`, which COUPLES them: with arm j at round r the core index is
  # (j+r) mod n and the order position is (j-r) mod n, so
  #     core_index + position == 2j (mod n)
  # is CONSTANT for each arm. Every arm then samples only its own diagonal of the
  # position x core grid -- n of the n^2 combinations -- and since both position
  # and core carry real cost differences, each arm averages over a different
  # subset. That is a systematic bias that LOOKS like it has been rotated away.
  # It is the residual that survived the order-rotation fix: nulls stayed around
  # +1% and always positive.
  #
  # Advancing cores once per COMPLETE order cycle (r / ARM_COUNT) decouples them,
  # so over ARM_COUNT^2 rounds every arm sees every (position, core) pair exactly
  # once. The ROUNDS guard enforces that multiple.
  local n=${#ARM_PIDS[@]} i shifted pid core observed
  for i in $(seq 0 $((n - 1))); do
    shifted=$(( (i + $1) % n ))
    pid="${ARM_PIDS[$i]}"
    core="${ARM_CORES[$shifted]}"
    taskset -cp "$core" "$pid" >/dev/null 2>&1 || {
      echo "PREFLIGHT FAIL: unable to pin arm pid $pid to core $core" >&2
      exit 6
    }
    observed=$(taskset -pc "$pid" 2>/dev/null | sed -n 's/.*affinity list: //p')
    if [[ "$observed" != "$core" ]]; then
      echo "PREFLIGHT FAIL: arm pid $pid affinity ${observed:-unknown}, expected $core" >&2
      exit 6
    fi
  done
}

# ROUNDS MUST BE A MULTIPLE OF THE ARM COUNT, or the rotation does not cancel.
# Each arm visits each core once per full cycle; a partial final cycle leaves
# some arm with an extra turn on a faster core, which shows up as a BIASED A/A
# null. Observed for real: 15 rounds with 3 arms (a multiple) gave a null of
# 1.0000, and 15 rounds with 4 arms gave 1.0144 with a CI excluding 1.0, which
# correctly voided that run's A/B verdict.
ARM_COUNT=${#ARM_PIDS[@]}
# ROUNDS must be a multiple of ARM_COUNT SQUARED. One factor of ARM_COUNT gives
# every arm every ORDER POSITION; the second gives every arm every CORE at each
# of those positions. A multiple of ARM_COUNT alone was not enough -- see
# rotate_cores.
CYCLE=$(( ARM_COUNT * ARM_COUNT ))
if [ $((ROUNDS % CYCLE)) -ne 0 ]; then
  ADJUSTED=$(( (ROUNDS / CYCLE + 1) * CYCLE ))
  echo "NOTE: rounds $ROUNDS is not a multiple of ${ARM_COUNT}^2=$CYCLE; raising to $ADJUSTED"
  echo "      so every arm sees every (position, core) pair equally often."
  ROUNDS=$ADJUSTED
fi

for r in $(seq 1 "$ROUNDS"); do
  # Cores advance once per COMPLETE order cycle, not once per round -- see
  # rotate_cores for why sharing the index with the order rotation is a trap.
  rotate_cores $(( r / ARM_COUNT ))
  # CYCLIC rotation of arm ORDER -- not the reversal this used to do.
  #
  # Reversing (A,A2,R -> R,A2,A) swaps only first and last: with three arms fr_A2
  # sits in position 2 in EVERY round, while fr_A alternates between first and
  # last. Position is not neutral -- whichever arm runs first in a round pays for
  # a colder cache and a just-idled core -- so a fixed-position arm accumulates a
  # systematic offset. That is measurable and was: with byte-identical binaries
  # on both arms, four -P 1 runs gave nulls of 1.0440, 1.0280, 1.0246, 1.0022,
  # always in the SAME direction (fr_A2 faster), two of them inadmissible, bias
  # to 7.22%. It is the defect behind the retracted -P 16 result too, which I had
  # wrongly blamed on pipelining.
  #
  # Rotating by the round index gives every arm every position an equal number of
  # times over each full cycle, so position cancels in the median instead of
  # accumulating. ROUNDS is already forced to a multiple of the arm count above,
  # which is exactly the condition that makes the cycle complete.
  ORDER_PORTS=("$FR_A_PORT" "$FR_A2_PORT" "$RD_PORT")
  ORDER_TAGS=(fr_A fr_A2 redis)
  if [ -n "$FR_BIN_B" ]; then
    ORDER_PORTS+=("$FR_B_PORT"); ORDER_TAGS+=(fr_B)
  fi
  n_arms=${#ORDER_PORTS[@]}
  for k in $(seq 0 $((n_arms - 1))); do
    idx=$(( (k + r) % n_arms ))
    run_arm "${ORDER_PORTS[$idx]}" "${ORDER_TAGS[$idx]}" "$r"
  done
  echo "  round $r done"
done

echo
python3 - "$RES" "$CLIENTS" <<'PY'
import sys, statistics as st, random
from collections import defaultdict
rounds = defaultdict(dict)
for line in open(sys.argv[1]):
    parts = line.rstrip("\n").split("\t")
    if len(parts) != 3:
        continue
    r, tag, rps = parts
    try:
        rounds[r][tag] = float(rps)
    except ValueError:
        pass

def ratios(num, den):
    return [v[num] / v[den] for v in rounds.values()
            if num in v and den in v and v[den] > 0]

def boot_ci(vals, iters=20000, seed=12345):
    """Bootstrap 95% CI of the MEDIAN. The per-round spread is not the error bar
    on the statistic we report -- the median of n rounds is far better determined
    than any single round -- and the ledger gate requires this interval, not a CV."""
    if len(vals) < 2:
        return (float("nan"), float("nan"))
    rng = random.Random(seed)
    meds = []
    n = len(vals)
    for _ in range(iters):
        meds.append(st.median([vals[rng.randrange(n)] for _ in range(n)]))
    meds.sort()
    return (meds[int(0.025 * iters)], meds[int(0.975 * iters)])

def show(label, num, den, note=""):
    rs = ratios(num, den)
    if not rs:
        return None
    m = st.median(rs)
    lo, hi = boot_ci(rs)
    print(f"{label:<26}{m:>9.4f}  bootstrap95 CI [{lo:.4f}, {hi:.4f}]  "
          f"[min {min(rs):.4f}, max {max(rs):.4f}] n={len(rs)} {note}")
    return (m, lo, hi)

print(f"{'arm':<26}{'ops/s':>12}")
for tag in ("fr_A", "fr_A2", "fr_B", "redis"):
    vals = [v[tag] for v in rounds.values() if tag in v]
    if vals:
        print(f"{tag:<26}{st.median(vals):>12,.0f}")
print()
print(f"{'ratio':<26}{'median':>9}")
null = show("A/A null  fr_A2/fr_A", "fr_A2", "fr_A", "<- control, want ~1.0")
eff  = show("A/B effect fr_B/fr_A", "fr_B", "fr_A")
ca   = show("COMPETITIVE fr_A/redis", "fr_A", "redis", "<- campaign number")
cb   = show("COMPETITIVE fr_B/redis", "fr_B", "redis")

print()
if null:
    # The decision rule is CI-based: the null's CI must contain 1.0 (or it is not
    # a null at all), and the effect's CI must EXCLUDE 1.0 and clear the null's
    # widest bound. A median that merely differs is not enough.
    null_ok = null[1] <= 1.0 <= null[2]
    widest = max(abs(null[1] - 1.0), abs(null[2] - 1.0))
    print(f"A/A null CI {'CONTAINS' if null_ok else 'EXCLUDES'} 1.0 "
          f"(widest bound {widest*100:.2f}% from 1.0)"
          f"{'' if null_ok else '  <- HARNESS IS BIASED; no A/B verdict is admissible'}")
    if eff:
        excludes = not (eff[1] <= 1.0 <= eff[2])
        clears = abs(eff[0] - 1.0) > widest
        if excludes and clears and null_ok:
            print(f"A/B effect {(eff[0]-1)*100:+.2f}%: CI excludes 1.0 and clears the "
                  f"null bound -> ADMISSIBLE")
        else:
            why = []
            if not excludes: why.append("its CI contains 1.0")
            if not clears:   why.append("it is inside the null's own bound")
            print(f"A/B effect {(eff[0]-1)*100:+.2f}%: NOT A RESULT ({'; '.join(why)})")
if ca:
    print(f"\nfr is at {ca[0]:.3f}x of vendored redis on this EVAL workload.")
    print("A self-speedup does not change this number unless it moves; that is the "
          "whole point of printing them together.")
PY
