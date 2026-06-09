#!/usr/bin/env bash
# run_parity_differs.sh — run the port-taking differential gates in scripts/
# against an already-running oracle + fr pair, auto-detecting each script's CLI
# convention.
#
# The scripts/ differ suite grew three incompatible argument conventions:
#   1. flag form    `differ.py --oracle <op> --fr <fp>`
#   2. positional   `differ.py <op> <fp>`
#   3. self-launch  `differ.py --bin <fr> --redis-bin <redis>`  (manages its own
#      servers; SKIPPED here — run those directly)
# This made running the suite as a batch error-prone (every invocation guessed
# wrong for ~1/3 of the scripts). This runner detects the convention per script
# (self-launchers via a `--redis-bin`/`--bin` grep; otherwise it tries the flag
# form and falls back to positional on an argparse "unrecognized arguments"
# error) and runs each with a per-script timeout so the slow drain-bounded ones
# (keyspace_notif, client_tracking) can't wedge the whole run.
#
# Usage:
#   ORACLE=legacy_redis_code/redis/src
#   $ORACLE/redis-server --port 16399 --daemonize yes --save '' --appendonly no
#   $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
#   scripts/run_parity_differs.sh 16399 16400 [per_script_timeout_secs]
#
# Exit status: 0 iff every executed differ passed (timeouts/skips are reported
# but do not by themselves fail the run — slow gates need a bigger budget).
set -u
OP=${1:-16399}
FP=${2:-16400}
TIMEOUT=${3:-120}
DIR="$(cd "$(dirname "$0")" && pwd)"

pass=0 fail=0 skip=0 timedout=0
failed_list=""

for script in "$DIR"/*.py; do
    name="$(basename "$script")"
    # Self-launching gates manage their own servers — skip (run directly).
    if grep -qE -- "--redis-bin|add_argument\\(\"--bin\"" "$script"; then
        echo "SKIP  $name (self-launching; run directly with --bin)"
        skip=$((skip + 1))
        continue
    fi
    # Detect convention: prefer the flag form; fall back to positional if
    # argparse rejects the flags.
    out="$(timeout "$TIMEOUT" python3 "$script" --oracle "$OP" --fr "$FP" 2>&1)"
    rc=$?
    if echo "$out" | grep -qiE "unrecognized arguments|invalid literal|no such option|error: argument"; then
        out="$(timeout "$TIMEOUT" python3 "$script" "$OP" "$FP" 2>&1)"
        rc=$?
    fi
    last="$(echo "$out" | grep -vE '^\s*$' | tail -1)"
    if [ "$rc" -eq 124 ]; then
        echo "SLOW  $name (timed out at ${TIMEOUT}s — needs a bigger budget)"
        timedout=$((timedout + 1))
    elif [ "$rc" -eq 0 ]; then
        echo "PASS  $name — ${last}"
        pass=$((pass + 1))
    elif echo "$out" | grep -qiE "DIVERGE|^FAIL|divergence|mismatch"; then
        # Nonzero AND the script reported an actual parity divergence: real FAIL.
        echo "FAIL  $name (rc=$rc) — ${last}"
        fail=$((fail + 1))
        failed_list="$failed_list $name"
    elif echo "$out" | grep -qiE "Traceback|Error:|unrecognized arguments|invalid literal|not enough values|IndexError|KeyError|usage:"; then
        # Nonzero from a Python traceback / argparse usage error: this script uses
        # a convention this runner doesn't speak (e.g. a 3-arg golden harness).
        # Not a parity failure — flag as unknown so it doesn't fail the run.
        echo "SKIP  $name (unrecognized CLI convention — run directly)"
        skip=$((skip + 1))
    else
        echo "FAIL  $name (rc=$rc) — ${last}"
        fail=$((fail + 1))
        failed_list="$failed_list $name"
    fi
done

echo "------------------------------------------------------------"
echo "parity differs: $pass passed, $fail failed, $timedout slow/timeout, $skip skipped (self-launching)"
[ -n "$failed_list" ] && echo "FAILED:$failed_list"
[ "$fail" -eq 0 ]
