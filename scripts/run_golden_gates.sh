#!/usr/bin/env bash
# run_golden_gates.sh — run the self-contained "golden" parity proofs (the
# scripts/*_golden.py files) as a suite.
#
# Each golden gate replays a FIXED command transcript that pins a SPECIFIC
# shipped edge-case fix — e.g. LSET 32-bit index truncation (4zv7a), the
# APPEND/DECR borrowed fast-path isomorphism, GEO/ZSET-op wrongtype ordering,
# XAUTOCLAIM/XRANGE/XGROUP SETID stream-COUNT edges, ZRANGE LIMIT — and asserts
# fr's output sha256 equals the redis 7.2.4 oracle's. Unlike the port-pair
# differs, these are SELF-CONTAINED golden proofs that hardcode their own server
# ports and use three incompatible CLI conventions, so run_parity_differs.sh
# SKIPs every one of them ("unrecognized CLI convention") and NOTHING ran them
# automatically — a regression in any of those fixes would slip in silently.
# This driver starts the exact servers each gate expects, runs all of them, and
# reports pass/fail.
#
# Three conventions are handled:
#   * oracle :18390 / fr :18391, hardcoded   (most gates)
#   * oracle :17380 / fr :17381, hardcoded   (hll_corrupt)
#   * `<gate> <oracle> <cand> <base>` 3-arg  (append/decr fast-path isomorphism)
#     — driven as `18390 18391 18391` so the PARITY (candidate==oracle) check is
#     what gates the run (the isomorphism check needs a separate baseline build
#     and is a no-op when candidate==baseline).
#
# Usage:
#   scripts/run_golden_gates.sh [fr_bin] [redis_bin]
#   (build fr first: CARGO_TARGET_DIR=/data/tmp/cargo-target cargo build -p fr-server)
#
# Exit status: 0 iff every golden gate passed.
set -u
FR_BIN=${1:-${CARGO_TARGET_DIR:-/data/tmp/cargo-target}/debug/frankenredis}
RD_BIN=${2:-legacy_redis_code/redis/src/redis-server}
DIR="$(cd "$(dirname "$0")" && pwd)"

PIDS=()
start_pair() { # oracle_port fr_port
    "$RD_BIN" --port "$1" --save '' --appendonly no --enable-debug-command yes \
        >"/tmp/golden_oracle_$1.log" 2>&1 &
    PIDS+=("$!")
    "$FR_BIN" --port "$2" --mode strict >"/tmp/golden_fr_$2.log" 2>&1 &
    PIDS+=("$!")
}
cleanup() {
    for pid in ${PIDS[@]+"${PIDS[@]}"}; do
        kill "$pid" 2>/dev/null
    done
}
trap cleanup EXIT

wait_ready() { # port
    for _ in $(seq 1 60); do
        if python3 -c "import socket;socket.create_connection(('127.0.0.1',$1),timeout=0.3)" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    echo "ERROR: server on port $1 did not become ready" >&2
    return 1
}

start_pair 18390 18391
start_pair 17380 17381
for p in 18390 18391 17380 17381; do wait_ready "$p" || exit 2; done

pass=0
fail=0
failed=""
check() { # name  invocation...
    local name="$1"
    shift
    local out
    out="$("$@" 2>&1)"
    # A golden gate passes when its embedded check prints an affirmative match —
    # "GOLDEN MATCH: True" (2-server gates) or "PARITY ...: True" (3-arg gates).
    if echo "$out" | grep -qE "GOLDEN MATCH: True|PARITY[^:]*: +True"; then
        echo "PASS  $name"
        pass=$((pass + 1))
    else
        echo "FAIL  $name — $(echo "$out" | grep -iE 'MATCH|PARITY|first diff|Error' | head -1)"
        fail=$((fail + 1))
        failed="$failed $name"
    fi
}

# Convention 1: hardcoded oracle :18390 / fr :18391.
for g in geo_wrongtype lset_index xautoclaim_count xgroup_setid xrange_count \
         zrange_limit zsetop_wrongtype; do
    check "${g}_golden.py" python3 "$DIR/${g}_golden.py"
done
# Convention 2: hardcoded oracle :17380 / fr :17381.
check "hll_corrupt_golden.py" python3 "$DIR/hll_corrupt_golden.py"
# Convention 3: 3-arg oracle/candidate/baseline — parity-only (cand==base==fr).
check "append_fastpath_golden.py" python3 "$DIR/append_fastpath_golden.py" 18390 18391 18391
check "decr_fastpath_golden.py" python3 "$DIR/decr_fastpath_golden.py" 18390 18391 18391

echo "------------------------------------------------------------"
echo "golden suite: $pass passed, $fail failed"
[ -n "$failed" ] && echo "FAILED:$failed"
[ "$fail" -eq 0 ]
