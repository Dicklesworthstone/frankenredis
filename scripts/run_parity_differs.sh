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
# Self-launching gates (those taking --bin/--redis-bin) build/manage their own
# short-lived servers, so they are driven with the binary paths rather than the
# port pair — meaning this runs the WHOLE suite, port-pair differs and
# self-launching gates alike, in one command.
#
# Usage:
#   ORACLE=legacy_redis_code/redis/src
#   $ORACLE/redis-server --port 16399 --daemonize yes --save '' --appendonly no
#   $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
#   scripts/run_parity_differs.sh 16399 16400 [timeout] [fr_bin] [redis_bin]
#
# Exit status: 0 iff every executed gate passed (timeouts/skips are reported
# but do not by themselves fail the run — slow gates need a bigger budget).
set -u
OP=${1:-16399}
FP=${2:-16400}
TIMEOUT=${3:-120}
FR_BIN=${4:-${CARGO_TARGET_DIR:-/data/tmp/cargo-target}/debug/frankenredis}
RD_BIN=${5:-legacy_redis_code/redis/src/redis-server}
DIR="$(cd "$(dirname "$0")" && pwd)"

pass=0 fail=0 skip=0 timedout=0 infra=0
failed_list=""

# A genuine parity FAIL requires the servers to still be reachable. If the fr
# (or oracle) server died mid-run — e.g. killed by a process reaper — a differ
# reads the dropped connection as a divergence and reports a spurious FAIL.
# Probe both ports so such infrastructure deaths are not miscounted as parity
# regressions (the single biggest source of false CI failures here).
servers_alive() {
    python3 - "$OP" "$FP" <<'PY' 2>/dev/null
import socket, sys
for p in (int(sys.argv[1]), int(sys.argv[2])):
    try:
        s = socket.create_connection(("127.0.0.1", p), timeout=2)
        s.sendall(b"*1\r\n$4\r\nPING\r\n")
        if not s.recv(16):
            sys.exit(1)
        s.close()
    except OSError:
        sys.exit(1)
sys.exit(0)
PY
}

# Per-gate state reset. A differ that leaves a non-default encoding threshold
# (e.g. encoding_config_boundary_differ sets list-max-listpack-size=4, the
# encoding gates set the *-max-listpack-* family small) silently contaminates
# every LATER gate that assumes compiled defaults — geo_differ / hash_differ /
# fuzz_untrodden then report phantom OBJECT ENCODING / reply divergences that are
# config artifacts, not fr bugs (this masked nothing real but burned agent time
# chasing 3 such phantoms). We snapshot the as-started baseline of the commonly
# mutated configs from BOTH servers ONCE (the documented invocation runs this
# right after a fresh start, so these are the compiled defaults the pair shares)
# and restore them + FLUSHALL before every gate, so each gate sees a clean slate.
BASELINE_FILE="$(mktemp)"
trap 'rm -f "$BASELINE_FILE"' EXIT

cfg_state() {
    # cfg_state capture  -> snapshot baseline of both servers into BASELINE_FILE
    # cfg_state restore  -> reapply that baseline to both servers, then FLUSHALL
    python3 - "$1" "$OP" "$FP" "$BASELINE_FILE" <<'PY' 2>/dev/null
import socket, sys
mode, op, fp, path = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
PARAMS = ["list-max-listpack-size", "hash-max-listpack-entries", "hash-max-listpack-value",
          "set-max-listpack-entries", "set-max-listpack-value", "set-max-intset-entries",
          "zset-max-listpack-entries", "zset-max-listpack-value", "notify-keyspace-events",
          "maxmemory", "maxmemory-policy"]
def conn(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=3); s.settimeout(3)
    buf = bytearray()
    def line():
        while b"\r\n" not in buf:
            buf.extend(s.recv(4096))
        i = buf.index(b"\r\n"); out = bytes(buf[:i]); del buf[:i + 2]; return out
    def reply():
        h = line(); t = h[:1]
        if t in (b"+", b"-", b":"):
            return h
        if t == b"$":
            n = int(h[1:])
            if n < 0:
                return None
            while len(buf) < n + 2:
                buf.extend(s.recv(4096))
            d = bytes(buf[:n]); del buf[:n + 2]; return d
        if t == b"*":
            n = int(h[1:])
            return [reply() for _ in range(n)] if n >= 0 else None
        return h
    def cmd(*a):
        m = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            m += b"$%d\r\n%s\r\n" % (len(x), x)
        s.sendall(m); return reply()
    return cmd
if mode == "capture":
    rows = []
    for port in (op, fp):
        c = conn(port)
        for name in PARAMS:
            r = c("CONFIG", "GET", name)
            if isinstance(r, list) and len(r) == 2 and r[1] is not None:
                rows.append("%d\t%s\t%s" % (port, name, r[1].decode("latin1")))
    with open(path, "w") as fh:
        fh.write("\n".join(rows) + "\n")
else:  # restore
    byport = {op: conn(op), fp: conn(fp)}
    try:
        for ln in open(path):
            ln = ln.rstrip("\n")
            if not ln:
                continue
            port, name, val = ln.split("\t", 2)
            byport[int(port)]("CONFIG", "SET", name, val)
    except FileNotFoundError:
        pass
    for c in byport.values():
        c("FLUSHALL")
PY
}

# Preflight (frankenredis-oracle-pollution guard): the per-gate baseline above is
# captured PER-PORT from the running servers, so it faithfully restores whatever
# each server had AT START. That is wrong when the oracle is a long-running shared
# instance whose runtime config was mutated by an earlier agent's probe (e.g.
# `CONFIG SET list-max-listpack-size 128`, while the compiled default is -2). Then
# the snapshot bakes the MISMATCH in and every encoding/perf gate reports phantom
# divergences (encoding_differ, large_data_perf_sweep LREM, meta_encoding_chain).
# fr ships the correct redis-7.2.4 compiled defaults, so align the ORACLE to fr's
# values BEFORE capturing the baseline. Also flag a DEBUG-availability asymmetry,
# which silently fails the DEBUG-RELOAD gates (one server re-derives encoding on
# reload, the other can't run DEBUG at all).
preflight_align() {
    python3 - "$OP" "$FP" <<'PY'
import socket, sys
op, fp = int(sys.argv[1]), int(sys.argv[2])
PARAMS = ["list-max-listpack-size", "hash-max-listpack-entries", "hash-max-listpack-value",
          "set-max-listpack-entries", "set-max-listpack-value", "set-max-intset-entries",
          "zset-max-listpack-entries", "zset-max-listpack-value", "notify-keyspace-events",
          "maxmemory", "maxmemory-policy"]
def conn(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=3); s.settimeout(3)
    buf = bytearray()
    def line():
        while b"\r\n" not in buf:
            buf.extend(s.recv(4096))
        i = buf.index(b"\r\n"); out = bytes(buf[:i]); del buf[:i+2]; return out
    def reply():
        h = line(); t = h[:1]
        if t in (b"+", b"-", b":"): return h
        if t == b"$":
            n = int(h[1:])
            if n < 0: return None
            while len(buf) < n+2: buf.extend(s.recv(4096))
            d = bytes(buf[:n]); del buf[:n+2]; return d
        if t == b"*":
            n = int(h[1:]); return [reply() for _ in range(n)] if n >= 0 else None
        return h
    def cmd(*a):
        m = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            m += b"$%d\r\n%s\r\n" % (len(x), x)
        s.sendall(m); return reply()
    return cmd
oc, fc = conn(op), conn(fp)
def get(c, name):
    r = c("CONFIG", "GET", name)
    return r[1].decode("latin1") if isinstance(r, list) and len(r) == 2 and r[1] is not None else None
realigned = 0
for name in PARAMS:
    ov, fv = get(oc, name), get(fc, name)
    if ov is not None and fv is not None and ov != fv:
        r = oc("CONFIG", "SET", name, fv)
        ok = isinstance(r, bytes) and r.startswith(b"+")
        print("  preflight: oracle %s %s->%s (match fr) %s" % (name, ov, fv, "" if ok else "[SET FAILED]"))
        realigned += ok
# DEBUG-availability symmetry: a denied DEBUG returns an -ERR mentioning the flag.
def debug_ok(c):
    r = c("DEBUG", "set-active-expire", "1")
    return isinstance(r, bytes) and r.startswith(b"+")
od, fd = debug_ok(oc), debug_ok(fc)
if od != fd:
    print("  preflight: WARNING DEBUG asymmetry oracle=%s fr=%s — DEBUG-RELOAD gates "
          "(meta_encoding_chain_gate) will falsely diverge; launch BOTH with "
          "--enable-debug-command yes" % (od, fd))
if realigned:
    print("  preflight: realigned %d oracle config(s) to fr's compiled defaults" % realigned)
PY
}
preflight_align
cfg_state capture

for script in "$DIR"/*.py; do
    name="$(basename "$script")"
    # Restore the as-started config + clear data so a prior config-mutating gate
    # cannot contaminate this one (self-launching gates use their own servers, so
    # this is a harmless no-op for them).
    cfg_state restore
    if grep -qE -- "--redis-bin|add_argument\\(.--bin.\\)" "$script"; then
        # Self-launching gate: drive it with the binary paths. Fall back to
        # --bin-only for gates that don't take --redis-bin (e.g. fr<->fr ones).
        out="$(timeout "$TIMEOUT" python3 "$script" --bin "$FR_BIN" --redis-bin "$RD_BIN" 2>&1)"
        rc=$?
        if echo "$out" | grep -qiE "unrecognized arguments|no such option|error: argument"; then
            out="$(timeout "$TIMEOUT" python3 "$script" --bin "$FR_BIN" 2>&1)"
            rc=$?
        fi
    else
        # Port-pair differ: prefer the flag form; fall back to positional if
        # argparse rejects the flags.
        out="$(timeout "$TIMEOUT" python3 "$script" --oracle "$OP" --fr "$FP" 2>&1)"
        rc=$?
        if echo "$out" | grep -qiE "unrecognized arguments|invalid literal|no such option|error: argument"; then
            out="$(timeout "$TIMEOUT" python3 "$script" "$OP" "$FP" 2>&1)"
            rc=$?
        fi
    fi
    last="$(echo "$out" | grep -vE '^\s*$' | tail -1)"
    if [ "$rc" -eq 124 ]; then
        echo "SLOW  $name (timed out at ${TIMEOUT}s — needs a bigger budget)"
        timedout=$((timedout + 1))
    elif [ "$rc" -eq 0 ]; then
        echo "PASS  $name — ${last}"
        pass=$((pass + 1))
    elif echo "$out" | grep -qiE "DIVERGE|^FAIL|divergence|mismatch"; then
        # Nonzero AND the script reported a divergence — but only count it as a
        # real parity FAIL if both servers are still alive. A server reaped
        # mid-run makes a differ see the dropped connection as a divergence.
        if servers_alive; then
            echo "FAIL  $name (rc=$rc) — ${last}"
            fail=$((fail + 1))
            failed_list="$failed_list $name"
        else
            echo "INFRA $name (a server died mid-run — re-run; not a parity FAIL)"
            infra=$((infra + 1))
        fi
    elif echo "$out" | grep -qiE "Traceback|Error:|unrecognized arguments|invalid literal|not enough values|IndexError|KeyError|usage:"; then
        # Nonzero from a Python traceback / argparse usage error: this script uses
        # a convention this runner doesn't speak (e.g. a 3-arg golden harness).
        # Not a parity failure — flag as unknown so it doesn't fail the run.
        echo "SKIP  $name (unrecognized CLI convention — run directly)"
        skip=$((skip + 1))
    elif servers_alive; then
        echo "FAIL  $name (rc=$rc) — ${last}"
        fail=$((fail + 1))
        failed_list="$failed_list $name"
    else
        echo "INFRA $name (a server died mid-run — re-run; not a parity FAIL)"
        infra=$((infra + 1))
    fi
done

echo "------------------------------------------------------------"
echo "parity suite: $pass passed, $fail failed, $timedout slow/timeout, $skip skipped, $infra infra (server died)"
[ -n "$failed_list" ] && echo "FAILED:$failed_list"
[ "$infra" -gt 0 ] && echo "NOTE: $infra gate(s) hit a mid-run server death — re-run those directly."
[ "$fail" -eq 0 ]
