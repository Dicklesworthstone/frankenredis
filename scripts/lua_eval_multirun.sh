#!/usr/bin/env bash
# lua_eval_multirun.sh — aggregate lua_eval_headtohead.sh ACROSS INVOCATIONS.
#
# WHY THIS EXISTS (frankenredis-lua-rediscall-loop-interpreter-bound-d3al0).
# The single-invocation harness bootstraps its CIs over ROUNDS. Measured, that is
# pseudo-replication: every round inside one invocation shares a per-run offset,
# so the rounds are correlated and the interval comes out far tighter than the
# measurement deserves. The evidence was that raising rounds from 18 to 36 made
# MORE runs inadmissible, not fewer -- 3 of 4 excluded 1.0 with bounds of only
# 1.3-2.9%, while the run medians themselves spread about 1.4% sd:
#
#     R=36 null medians   1.0084   1.0104   0.9918   1.0248
#     their within-run CIs  +/-0.4-1%   <- too narrow to contain that spread
#
# More rounds shrink the interval around each run's OWN offset. They cannot
# average the offset away, because it is constant within the run.
#
# So the unit of replication is the RUN. This script takes each invocation's
# median as ONE sample and bootstraps across those, which is the estimator the
# data actually supports. Expect WIDER and more honest intervals than the
# per-round CIs the inner harness prints -- if the two disagree, the inner one is
# wrong.
#
# Usage:
#   FR_BIN_A=... [FR_BIN_B=...] scripts/lua_eval_multirun.sh --runs 7 [harness args...]
set -euo pipefail

RUNS=7
PASSTHRU=()
while [ $# -gt 0 ]; do
  case "$1" in
    --runs) RUNS="$2"; shift 2;;
    *) PASSTHRU+=("$1"); shift;;
  esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"
TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT

echo "== $RUNS independent invocations; each contributes ONE median per ratio =="
for r in $(seq 1 "$RUNS"); do
  out=$(FR_BIN_A="${FR_BIN_A:-}" FR_BIN_B="${FR_BIN_B:-}" \
        bash "$HERE/lua_eval_headtohead.sh" "${PASSTHRU[@]}" 2>&1 || true)
  # Pull each ratio's median out of the inner harness's own report.
  # `|| true` on every extraction: under `set -euo pipefail` a grep that matches
  # nothing exits 1 and the command substitution takes the whole script down,
  # which is how the first version of this aggregator printed its banner and then
  # silently vanished. A missing ratio must degrade to VOID, not abort the batch.
  nul=$(echo "$out" | grep -oE 'A/A null  fr_A2/fr_A +[0-9.]+' | awk '{print $NF}' || true)
  eff=$(echo "$out" | grep -oE 'A/B effect fr_B/fr_A +[0-9.]+' | awk '{print $NF}' || true)
  ca=$(echo  "$out" | grep -oE 'COMPETITIVE fr_A/redis +[0-9.]+' | awk '{print $NF}' || true)
  cb=$(echo  "$out" | grep -oE 'COMPETITIVE fr_B/redis +[0-9.]+' | awk '{print $NF}' || true)
  if [ -z "$nul" ]; then
    echo "  run $r: VOID (no measurement produced)"
    echo "$out" | tail -3 | sed 's/^/      /'
    continue
  fi
  echo "  run $r: null=$nul effect=${eff:--} frA/redis=${ca:--} frB/redis=${cb:--}"
  echo "$nul ${eff:-nan} ${ca:-nan} ${cb:-nan}" >> "$TMP"
  sleep 8
done

echo
python3 - "$TMP" <<'PY'
import sys, statistics as st, random, math
rows = [l.split() for l in open(sys.argv[1]) if l.strip()]
if len(rows) < 3:
    print(f"INSUFFICIENT: only {len(rows)} usable runs; need >= 3 to say anything.")
    sys.exit(2)

def col(i):
    v = [float(r[i]) for r in rows if r[i] != 'nan' and not math.isnan(float(r[i]))]
    return v

def boot(vals, it=20000, seed=4242):
    r = random.Random(seed); n = len(vals)
    m = sorted(st.median([vals[r.randrange(n)] for _ in range(n)]) for _ in range(it))
    return m[int(.025*it)], m[int(.975*it)]

def show(label, i, note=""):
    v = col(i)
    if not v: return None
    med = st.median(v); lo, hi = boot(v)
    sd = st.pstdev(v) if len(v) > 1 else 0.0
    print(f"{label:<24}{med:>8.4f}  across-run 95% CI [{lo:.4f}, {hi:.4f}]  "
          f"sd={sd*100:.2f}%  n_runs={len(v)} {note}")
    return med, lo, hi

print(f"{'ratio':<24}{'median':>8}")
nul = show("A/A null", 0, "<- control")
eff = show("A/B effect", 1)
ca  = show("COMPETITIVE fr_A/redis", 2)
cb  = show("COMPETITIVE fr_B/redis", 3)
print()
if nul:
    ok = nul[1] <= 1.0 <= nul[2]
    widest = max(abs(nul[1]-1.0), abs(nul[2]-1.0))
    print(f"A/A null CI {'CONTAINS' if ok else 'EXCLUDES'} 1.0; widest bound {widest*100:.2f}%"
          f"{'' if ok else '   <- STILL BIASED across runs; no A/B verdict'}")
    if eff:
        excl = not (eff[1] <= 1.0 <= eff[2])
        clears = abs(eff[0]-1.0) > widest
        verdict = "ADMISSIBLE" if (ok and excl and clears) else "NOT A RESULT"
        print(f"A/B effect {(eff[0]-1)*100:+.2f}%: {verdict}")
PY
