#!/usr/bin/env bash
# run_fuzz_sweep.sh — run every differential FUZZER against an oracle+fr pair in one
# command. The fuzzers have INCONSISTENT arg conventions (some positional
# `<oracle> <fr>`, some `--oracle/--fr`), which is easy to get wrong; this records the
# correct invocation for each so the full ~150k-op differential fuzz can be re-run as a
# single step (e.g. after a fix, or to re-confirm saturation).
#
# Usage: scripts/run_fuzz_sweep.sh [ORACLE_PORT] [FR_PORT]
#   defaults: 28801 28802
# Exit non-zero if any fuzzer reports a divergence.
set -uo pipefail
ORACLE="${1:-28801}"
FR="${2:-28802}"
HERE="$(cd "$(dirname "$0")" && pwd)"
fail=0

run() {  # name + full arg list
  local name="$1"; shift
  if [ ! -f "$HERE/$name" ]; then echo "SKIP  $name (missing)"; return; fi
  echo "=== $name ==="
  if timeout 300 python3 "$HERE/$name" "$@" 2>&1 | tail -2; then :; else fail=1; fi
}

# Positional <oracle> <fr>:
run random_command_differ.py        "$ORACLE" "$FR"
# random_differential_fuzz reads argv as <seed> <N> [oracle_port] [fr_port] (its first two
# args are NOT ports); pass an explicit seed+N so it connects to THIS pair instead of erroring
# on its standalone default ports 28801/28802 (was a silent skip that false-failed the sweep).
run random_differential_fuzz.py     1234 8000 "$ORACLE" "$FR"
run fuzz_untrodden_differ.py        "$ORACLE" "$FR"
# --oracle/--fr flag style:
run option_fuzz_differ.py           --oracle "$ORACLE" --fr "$FR"
run random_state_differ.py          --oracle "$ORACLE" --fr "$FR" --seeds 6 --iters 3000
run random_reply_differ.py          --oracle "$ORACLE" --fr "$FR"

echo "================================================================"
if [ "$fail" -ne 0 ]; then
  echo "FUZZ SWEEP: at least one fuzzer reported a divergence (see above)"
  exit 1
fi
echo "FUZZ SWEEP: all differential fuzzers clean vs redis 7.2.4"
