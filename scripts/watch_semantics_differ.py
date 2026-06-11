#!/usr/bin/env python3
"""watch_semantics_differ.py — WATCH transaction-dirty semantics vs redis 7.2.4.

WATCH marks an EXEC for abort (nil) when a watched key is MODIFIED between WATCH
and EXEC. The basic "another connection SETs the watched key" case is covered by
multi_exec_differ, but the dirty signal has subtle edges that single-connection
replay can't reach because they need a SECOND connection and/or the passage of
time:
  * a no-op-looking write (SET to the SAME value, GETSET to the same value) still
    dirties — redis signals on the WRITE, not on a value change;
  * a watched key EXPIRING (lazily via another conn's read, actively, or just by
    EXEC-time detection) dirties it;
  * FLUSHDB / FLUSHALL dirty every watched key;
  * creating a watched key that did NOT exist at WATCH time dirties it;
  * RENAME onto / away, COPY onto, MOVE away, DEL all dirty;
  * a true no-op (PERSIST on a key with no TTL) and pure reads (GET / TYPE) do
    NOT dirty.
fr must match redis on every one or a transaction wrongly aborts (or wrongly
runs) under concurrency. Each case is checked by whether EXEC RAN (sentinel set)
or ABORTed (nil), compared fr vs the oracle.

SETUP (oracle config-less => compiled defaults; fr strict mode):
    legacy_redis_code/redis/src/redis-server --port 16399 --save '' --appendonly no --daemonize yes
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    scripts/watch_semantics_differ.py 16399 16400
"""
import socket
import sys
import time

ORACLE_DEFAULT = 16399
FR_DEFAULT = 16400


def _conn(p):
    s = socket.create_connection(("127.0.0.1", p))
    s.settimeout(3)
    return s, bytearray()


def _rd(s, b):
    while b"\r\n" not in b:
        b.extend(s.recv(8192))
    i = b.index(b"\r\n")
    h = bytes(b[:i])
    del b[: i + 2]
    t = h[:1]
    if t in (b"+", b"-", b":"):
        return h
    if t == b"$":
        n = int(h[1:])
        if n < 0:
            return None
        while len(b) < n + 2:
            b.extend(s.recv(8192))
        d = bytes(b[:n])
        del b[: n + 2]
        return d
    if t == b"*":
        n = int(h[1:])
        return None if n < 0 else [_rd(s, b) for _ in range(n)]
    return h


def _mk(*a):
    out = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        out += b"$%d\r\n%s\r\n" % (len(x), x)
    return out


def client(p):
    s, b = _conn(p)
    return lambda *a: (s.sendall(_mk(*a)), _rd(s, b))[1]


WK = "wk"


def run_case(p, setup, other, sleep_before=0.0, sleep_other=0.0):
    a, b = client(p), client(p)
    a("FLUSHALL")
    for c in setup:
        a(*c)
    a("WATCH", WK)
    a("MULTI")
    a("SET", "sentinel", "1")
    if sleep_before:
        time.sleep(sleep_before)
    for c in other:
        b(*c)
    if sleep_other:
        time.sleep(sleep_other)
    ex = a("EXEC")
    return "ABORT" if ex is None else "RAN"


# (name, setup-on-A-before-WATCH, other-conn-cmds, sleep_before, sleep_other)
CASES = [
    ("noop-set-same", [("SET", WK, "v")], [("SET", WK, "v")], 0, 0),
    ("create-watched-none", [], [("SET", WK, "new")], 0, 0),
    ("flushdb", [("SET", WK, "v")], [("FLUSHDB",)], 0, 0),
    ("flushall", [("SET", WK, "v")], [("FLUSHALL",)], 0, 0),
    ("del-watched", [("SET", WK, "v")], [("DEL", WK)], 0, 0),
    ("unlink-watched", [("SET", WK, "v")], [("UNLINK", WK)], 0, 0),
    ("rename-onto", [("SET", WK, "v"), ("SET", "src", "x")], [("RENAME", "src", WK)], 0, 0),
    ("rename-away", [("SET", WK, "v")], [("RENAME", WK, "other")], 0, 0),
    ("expire-set", [("SET", WK, "v")], [("EXPIRE", WK, "1000")], 0, 0),
    ("persist-noop", [("SET", WK, "v")], [("PERSIST", WK)], 0, 0),
    ("getset-same", [("SET", WK, "v")], [("GETSET", WK, "v")], 0, 0),
    ("append", [("SET", WK, "v")], [("APPEND", WK, "x")], 0, 0),
    ("lpush-lpop-back", [("RPUSH", WK, "a")], [("LPUSH", WK, "z"), ("LPOP", WK)], 0, 0),
    ("copy-onto", [("SET", WK, "v"), ("SET", "src", "x")], [("COPY", "src", WK, "REPLACE")], 0, 0),
    ("move-away", [("SET", WK, "v")], [("MOVE", WK, "2")], 0, 0),
    ("read-only-get", [("SET", WK, "v")], [("GET", WK)], 0, 0),
    ("type-only", [("SET", WK, "v")], [("TYPE", WK)], 0, 0),
    # temporal: watched key expires before EXEC, via lazy read / active / passive.
    ("expire-lazy", [("SET", WK, "v", "PX", "100")], [("GET", WK)], 0.25, 0),
    ("expire-active", [("SET", WK, "v", "PX", "100")], [], 0.25, 0.6),
    ("expire-passive", [("SET", WK, "v", "PX", "100")], [], 0.25, 0),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else ORACLE_DEFAULT
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else FR_DEFAULT
    div = 0
    for name, setup, other, sb, so in CASES:
        ro = run_case(op, setup, other, sb, so)
        rf = run_case(fp, setup, other, sb, so)
        if ro != rf:
            div += 1
            print(f"DIVERGE {name}: oracle={ro} fr={rf}")
    print("-" * 60)
    print(f"checked {len(CASES)} WATCH-dirty semantics cases; divergences: {div}")
    if div == 0:
        print("PASS — fr WATCH transaction-dirty semantics match redis 7.2.4")
        return 0
    print(f"FAIL — {div} divergence(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
