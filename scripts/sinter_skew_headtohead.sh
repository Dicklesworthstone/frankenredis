#!/usr/bin/env bash
# sinter_skew_headtohead.sh — SINTERSTORE on a SKEWED pair of intsets, fr vs
# vendored Redis, with a same-invocation A/A null.
#
# WHY. `intersect_sorted_i64`'s skewed branch (large:small ratio >= 32) is the
# code path `frankenredis-6irj9` found regressed: its per-element probe had been
# swapped to a branchless one that measured 0.75-0.83x against the plain binary
# search it replaced, in BOTH the hit-heavy and disjoint regimes. The in-repo
# unit A/B proves the fix restores that path; this says where the resulting
# command actually stands against the incumbent, which a self-comparison cannot.
#
# Discipline is inherited from scripts/lua_eval_headtohead.sh, including the
# three defects found the hard way there and fixed here too:
#   * core 0 is skipped and core assignment ROTATES every round, because a fixed
#     assignment hands one arm a permanently slower core and shows up as a
#     biased A/A null that more samples do not shrink;
#   * ROUNDS is raised to a multiple of the arm count, since round-robin
#     debiasing only cancels over whole cycles;
#   * the rps is parsed from `-q`, never `--csv`, whose columns move when the
#     benchmarked command contains commas or quotes.
#
# Every arm must return the same SINTERSTORE answer before any timing is
# believed: redis-benchmark counts an ERROR REPLY as a completed request, so an
# engine that refused the command would post an excellent number.
#
# Usage: FR_BIN_A=/tmp/fr_base [FR_BIN_B=/tmp/fr_cand] scripts/sinter_skew_headtohead.sh
#          -n <requests> -c <clients> -R <rounds> --small <n> --big <n>
set -euo pipefail

REQUESTS=20000; CLIENTS=1; ROUNDS=12; SMALL=5000; BIG=500000
FR_BIN_A="${FR_BIN_A:-/tmp/fr_sinter_a}"
FR_BIN_B="${FR_BIN_B:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    -n) REQUESTS="$2"; shift 2;;
    -c) CLIENTS="$2"; shift 2;;
    -R) ROUNDS="$2"; shift 2;;
    --small) SMALL="$2"; shift 2;;
    --big) BIG="$2"; shift 2;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH="$ROOT/legacy_redis_code/redis/src/redis-benchmark"
REDIS="$ROOT/legacy_redis_code/redis/src/redis-server"
CLI="$ROOT/legacy_redis_code/redis/src/redis-cli"
A_PORT=27591; A2_PORT=27592; RD_PORT=27593; B_PORT=27594

nproc_n=$(nproc)
pick_cores() {
  local want="$1" out=() c sib u v
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
ALL=$(pick_cores "$NEED") || { echo "PREFLIGHT FAIL: fewer than $NEED quiet cores" >&2; exit 6; }
if [ -n "$FR_BIN_B" ]; then
  IFS=, read -r A_CORE A2_CORE RD_CORE B_CORE CLIENT_CORE <<<"$ALL"
else
  IFS=, read -r A_CORE A2_CORE RD_CORE CLIENT_CORE <<<"$ALL"; B_CORE=""
fi

FR_BIN_A2=/tmp/fr_sinter_a2_null
cp "$FR_BIN_A" "$FR_BIN_A2"
cmp -s "$FR_BIN_A" "$FR_BIN_A2" || { echo "FAIL: null arm not byte-identical" >&2; exit 5; }
if [ -n "$FR_BIN_B" ] && cmp -s "$FR_BIN_A" "$FR_BIN_B"; then
  echo "FAIL: candidate is byte-identical to baseline" >&2; exit 5
fi
"$REDIS" --version | head -1
echo "host $(hostname) load $(cut -d' ' -f1-3 /proc/loadavg)"
echo "cores: A=$A_CORE A2=$A2_CORE redis=$RD_CORE B=${B_CORE:-none} client=$CLIENT_CORE"
echo "shape: |small|=$SMALL |big|=$BIG ratio=$((BIG / SMALL)) (skewed branch needs >= 32)"

PORTS="$A_PORT $A2_PORT $RD_PORT"; [ -n "$FR_BIN_B" ] && PORTS="$PORTS $B_PORT"
for p in $PORTS; do
  ss -ltn 2>/dev/null | grep -q ":$p " && { echo "PREFLIGHT FAIL: port $p bound" >&2; exit 4; }
done

PIDS=""
cleanup() { for p in $PIDS; do kill -9 "$p" 2>/dev/null || true; done; }
trap cleanup EXIT
# set-max-intset-entries defaults to 512, above which an integer set stops being
# intset-encoded -- so at this shape the skewed `intersect_sorted_i64` branch is
# NOT reached under stock config. It is raised via CONFIG SET after startup
# (below), NOT on the command line: redis accepts `--set-max-intset-entries` as
# an argv flag and fr rejects it ("unknown argument"), so the argv form starts
# redis happily and kills all three fr arms. CONFIG SET is accepted by both.
# The encoding is asserted after seeding rather than assumed.
INTSET_MAX=$(( BIG * 2 ))
taskset -c "$A_CORE"  "$FR_BIN_A"  --port $A_PORT  >/tmp/ssh_a.log  2>&1 & PIDS="$PIDS $!"
taskset -c "$A2_CORE" "$FR_BIN_A2" --port $A2_PORT >/tmp/ssh_a2.log 2>&1 & PIDS="$PIDS $!"
taskset -c "$RD_CORE" "$REDIS" --port $RD_PORT --save '' --appendonly no >/tmp/ssh_rd.log 2>&1 & PIDS="$PIDS $!"
if [ -n "$FR_BIN_B" ]; then
  taskset -c "$B_CORE" "$FR_BIN_B" --port $B_PORT >/tmp/ssh_b.log 2>&1 & PIDS="$PIDS $!"
fi
sleep 2

echo "== self-reported running-image SHA-256 (sha256sum of /proc/<pid>/exe) =="
# `|| true` on both reads: under `set -e` a failed /proc read would abort the
# whole run, and a missing provenance line must not cost a measurement.
rimg() { local s
  s=$(sudo -n sha256sum "/proc/$1/exe" 2>/dev/null | awk '{print $1}' || true)
  [ -z "$s" ] && s=$(sha256sum "/proc/$1/exe" 2>/dev/null | awk '{print $1}' || true)
  echo "  $2 benchmarked server ELF self-reported SHA-256 ${s:-UNAVAILABLE} (pid $1)"; }
set -- $PIDS
rimg "$1" fr_A; rimg "$2" fr_A2; rimg "$3" redis
[ -n "$FR_BIN_B" ] && rimg "$4" fr_B

# Seed the skewed pair. `small` is a strided subset of `big`, so the intersection
# is exactly |small| -- a 100%-hit skewed probe, the regime the regression was
# worst in.
#
# Seeding is CHUNKED at 20k SADDs per EVAL. A single script covering |big| trips
# fr's per-script iteration cap, and because the cap fires AFTER the first loop
# has run, the naive one-script version silently left `big` populated and `small`
# empty -- which benchmarks perfectly and answers SINTERCARD=0. The equality
# probe below is what caught it.
seed_chunked() {  # PORT KEY COUNT
  local off=0
  while [ "$off" -lt "$3" ]; do
    local n=$(( $3 - off )); [ "$n" -gt 20000 ] && n=20000
    "$CLI" -p "$1" eval \
      "local o=tonumber(ARGV[1]) for i=0,tonumber(ARGV[2])-1 do redis.call('SADD', KEYS[1], (o+i)*2) end return 1" \
      1 "$2" "$off" "$n" >/dev/null
    off=$(( off + n ))
  done
}
EXPECT=""
for p in $PORTS; do
  "$CLI" -p "$p" ping >/dev/null 2>&1 || { echo "FAIL: port $p not up"; exit 1; }
  "$CLI" -p "$p" config set set-max-intset-entries "$INTSET_MAX" >/dev/null
  seed_chunked "$p" big "$BIG"
  seed_chunked "$p" small "$SMALL"
  enc=$("$CLI" -p "$p" object encoding big)
  # SINTERSTORE, deliberately, NOT SINTERCARD. Only ONE production caller reaches
  # the code under test: SetValue::retain_intersect -> intersect_sorted_i64, used
  # by SINTER/SINTERSTORE. SINTERCARD has its OWN membership loop calling
  # `intset_binary_search_contains` directly and never enters that function, so a
  # SINTERCARD harness measures a path this change does not touch -- which is
  # exactly what the first version of this script did, reporting a +1.87% that
  # could not have come from the diff. SINTERSTORE keeps the reply to an integer
  # so the intersection, not the 5000-element reply encoding, dominates.
  got=$("$CLI" -p "$p" sinterstore dst small big 2>&1)
  echo "  port $p: SINTERSTORE=$got  |big| encoding=$enc"
  [ "$got" = "$SMALL" ] || { echo "FAIL: port $p answered $got, expected $SMALL" >&2; exit 5; }
  [ "$enc" = "intset" ] || { echo "FAIL: |big| is '$enc', not intset -- the skewed intset branch is not being exercised, so any ratio here would be measuring a different code path" >&2; exit 5; }
  [ -z "$EXPECT" ] && EXPECT="$got"
  [ "$got" = "$EXPECT" ] || { echo "FAIL: engines disagree ($got vs $EXPECT)" >&2; exit 5; }
done

RES=/tmp/ssh_res.tsv; : > "$RES"
run_arm() {  # PORT TAG ROUND
  local rps
  rps=$(taskset -c "$CLIENT_CORE" "$BENCH" -p "$1" -n "$REQUESTS" -c "$CLIENTS" \
          -q sinterstore dst small big 2>/dev/null \
        | tr '\r' '\n' | grep -oE '[0-9]+\.[0-9]+ requests per second' | tail -1 | awk '{print $1}')
  [ -z "$rps" ] && { echo "FAIL: no rps for $2" >&2; exit 5; }
  echo -e "$3\t$2\t$rps" >> "$RES"
}

ARM_PIDS=(); read -r -a ARM_PIDS <<<"$(echo $PIDS)"
if [ -n "$FR_BIN_B" ]; then ARM_CORES=("$A_CORE" "$A2_CORE" "$RD_CORE" "$B_CORE")
else ARM_CORES=("$A_CORE" "$A2_CORE" "$RD_CORE"); fi
ARM_COUNT=${#ARM_PIDS[@]}
if [ $((ROUNDS % ARM_COUNT)) -ne 0 ]; then
  ROUNDS=$(( (ROUNDS / ARM_COUNT + 1) * ARM_COUNT ))
  echo "NOTE: rounds raised to $ROUNDS, a multiple of the $ARM_COUNT arms, so rotation cancels"
fi
rotate_cores() { local n=${#ARM_PIDS[@]} i sh
  for i in $(seq 0 $((n - 1))); do sh=$(( (i + $1) % n ))
    taskset -cp "${ARM_CORES[$sh]}" "${ARM_PIDS[$i]}" >/dev/null 2>&1 || true; done; }

for r in $(seq 1 "$ROUNDS"); do
  rotate_cores "$r"
  if [ $((r % 2)) -eq 1 ]; then
    run_arm $A_PORT fr_A "$r"; run_arm $A2_PORT fr_A2 "$r"; run_arm $RD_PORT redis "$r"
    [ -n "$FR_BIN_B" ] && run_arm $B_PORT fr_B "$r"
  else
    [ -n "$FR_BIN_B" ] && run_arm $B_PORT fr_B "$r"
    run_arm $RD_PORT redis "$r"; run_arm $A2_PORT fr_A2 "$r"; run_arm $A_PORT fr_A "$r"
  fi
  echo "  round $r done"
done

echo
python3 - "$RES" <<'PY'
import sys, statistics as st, random
from collections import defaultdict
rounds = defaultdict(dict)
for line in open(sys.argv[1]):
    p = line.rstrip("\n").split("\t")
    if len(p) == 3:
        try: rounds[p[0]][p[1]] = float(p[2])
        except ValueError: pass

def boot(v, it=20000, seed=99):
    if len(v) < 2: return (float("nan"),) * 2
    r = random.Random(seed); n = len(v)
    m = sorted(st.median([v[r.randrange(n)] for _ in range(n)]) for _ in range(it))
    return m[int(.025 * it)], m[int(.975 * it)]

def show(label, num, den, note=""):
    rs = [v[num] / v[den] for v in rounds.values() if num in v and den in v and v[den] > 0]
    if not rs: return None
    m = st.median(rs); lo, hi = boot(rs)
    print(f"{label:<26}{m:>9.4f}  bootstrap95 CI [{lo:.4f}, {hi:.4f}]  n={len(rs)} {note}")
    return (m, lo, hi)

print(f"{'arm':<26}{'ops/s':>12}")
for tag in ("fr_A", "fr_A2", "fr_B", "redis"):
    v = [x[tag] for x in rounds.values() if tag in x]
    if v: print(f"{tag:<26}{st.median(v):>12,.0f}")
print()
null = show("A/A null  fr_A2/fr_A", "fr_A2", "fr_A", "<- control, want ~1.0")
eff  = show("A/B effect fr_B/fr_A", "fr_B", "fr_A")
ca   = show("COMPETITIVE fr_A/redis", "fr_A", "redis", "<- campaign number")
cb   = show("COMPETITIVE fr_B/redis", "fr_B", "redis")
print()
if null:
    ok = null[1] <= 1.0 <= null[2]
    widest = max(abs(null[1] - 1.0), abs(null[2] - 1.0))
    print(f"A/A null CI {'CONTAINS' if ok else 'EXCLUDES'} 1.0 (widest bound {widest*100:.2f}%)"
          f"{'' if ok else '  <- BIASED; no A/B verdict admissible'}")
    if eff:
        excl = not (eff[1] <= 1.0 <= eff[2])
        if excl and abs(eff[0]-1.0) > widest and ok:
            print(f"A/B effect {(eff[0]-1)*100:+.2f}%: ADMISSIBLE")
        else:
            print(f"A/B effect {(eff[0]-1)*100:+.2f}%: NOT A RESULT")
if ca: print(f"\nfr is at {ca[0]:.3f}x of vendored redis on skewed SINTERSTORE.")
PY
