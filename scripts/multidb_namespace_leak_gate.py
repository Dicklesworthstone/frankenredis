#!/usr/bin/env python3
"""multidb_namespace_leak_gate.py — non-zero-DB error/parity gate vs redis 7.2.4.

fr namespaces keys internally as `\\0frdb\\0<db>\\0<key>` in non-zero databases.
A whole class of bugs is an error formatter (or reply builder) that echoes the
RAW namespaced key instead of decoding it — e.g. the stream NOGROUP path once
leaked `\\0frdb\\0...` for a SELECTed DB (fixed caad636a3 via decode_db_key in 4
formatters). Any NEW error formatter that interpolates a key/group/consumer name
can regress this, and it only shows up in a non-zero DB.

This gate runs, inside DB 5, a battery of error-producing and key-echoing
commands (WRONGTYPE, NOGROUP, missing-key, MOVE/COPY/SWAPDB cross-DB, KEYS/SCAN)
and asserts, for every reply:
  (1) byte-exact equality with a config-less redis 7.2.4 oracle (KEYS/SCAN are
      compared order-insensitively since redis dict order is unspecified), AND
  (2) fr's reply contains NO internal-namespace sentinel (`frdb`, `\\x00frdb`).

Usage: multidb_namespace_leak_gate.py <oracle_port> <fr_port>
Exit 0 if all match and no leak; 1 otherwise.
"""
import socket
import sys
import time

SENTINELS = (b"frdb", b"\x00frdb\x00")


def cli(p):
    return socket.create_connection(("127.0.0.1", p), timeout=3)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        if isinstance(x, str):
            x = x.encode()
        elif isinstance(x, int):
            x = str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.015)
    return s.recv(131072)


CASES = []


def add(label, setup, probe):
    CASES.append((label, setup, probe))


SEL = [("SELECT", "5")]

# Error/key-echoing commands inside a non-zero DB (the leak-prone surface).
add("nogroup err", SEL + [("XADD", "st", "1-1", "f", "v")], ("XREADGROUP", "GROUP", "nog", "c", "STREAMS", "st", ">"))
add("xclaim nogroup", SEL + [("XADD", "st", "1-1", "f", "v")], ("XCLAIM", "st", "nog", "c", "0", "1-1"))
add("xack nogroup", SEL + [("XADD", "st", "1-1", "f", "v")], ("XACK", "st", "nog", "1-1"))
add("xsetid nostream", SEL, ("XSETID", "nope", "5-5"))
add("xinfo nostream", SEL, ("XINFO", "STREAM", "nope"))
add("xgroup create nostream", SEL, ("XGROUP", "CREATE", "nope", "g", "0"))
add("wrongtype get", SEL + [("RPUSH", "k", "x")], ("GET", "k"))
add("incr wrongtype", SEL + [("RPUSH", "k", "x")], ("INCR", "k"))
add("lpush wrongtype", SEL + [("SET", "k", "v")], ("LPUSH", "k", "x"))
add("getex wrongtype", SEL + [("RPUSH", "k", "x")], ("GETEX", "k"))
add("smove wrongtype", SEL + [("SET", "s", "v"), ("SADD", "s2", "x")], ("SMOVE", "s", "s2", "m"))
add("expire missing", SEL, ("EXPIRE", "nope", "100"))
add("object encoding missing", SEL, ("OBJECT", "ENCODING", "nope"))
add("debug object missing", SEL, ("DEBUG", "OBJECT", "nope"))

# Cross-DB operations.
add("move to db6", SEL + [("SET", "k", "v")], ("MOVE", "k", "6"))
add("move existing dest", SEL + [("SET", "k", "v"), ("SELECT", "6"), ("SET", "k", "o"), ("SELECT", "5")], ("MOVE", "k", "6"))
add("move missing", SEL, ("MOVE", "nope", "6"))
add("move same db", SEL + [("SET", "k", "v")], ("MOVE", "k", "5"))
add("move bad db", SEL + [("SET", "k", "v")], ("MOVE", "k", "99"))
add("copy to db", SEL + [("SET", "k", "v")], ("COPY", "k", "k2", "DB", "6"))
add("copy same db same key", SEL + [("SET", "k", "v")], ("COPY", "k", "k", "DB", "5"))
add("select out of range", [], ("SELECT", "99"))
add("select notanum", [], ("SELECT", "abc"))
add("swapdb same", [], ("SWAPDB", "0", "0"))
add("swapdb bad", [], ("SWAPDB", "0", "99"))
add("flushdb bad arg", SEL, ("FLUSHDB", "BADARG"))

# Key-listing inside a non-zero DB (must NOT show internal namespace).
add("dbsize", SEL + [("SET", "a", "1"), ("SET", "b", "2")], ("DBSIZE",))
add("keys glob", SEL + [("SET", "abc", "1"), ("SET", "abd", "2"), ("SET", "xyz", "3")], ("KEYS", "ab*"))
add("keys all", SEL + [("SET", "k1", "1"), ("SET", "k2", "2")], ("KEYS", "*"))
add("scan match", SEL + [("MSET", "k1", "1", "k2", "2", "k3", "3")], ("SCAN", "0", "MATCH", "k*"))
add("randomkey type", SEL + [("SET", "only", "1")], ("TYPE", "only"))


def run(port):
    s = cli(port)
    out = []
    for label, setup, probe in CASES:
        cmd(s, "SELECT", "0")
        cmd(s, "FLUSHALL")
        for c in setup:
            cmd(s, *c)
        out.append((label, cmd(s, *probe)))
        cmd(s, "SELECT", "0")
    s.close()
    return out


ORDER_INSENSITIVE = {"keys glob", "keys all", "scan match"}


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(1)
    oport, fport = int(sys.argv[1]), int(sys.argv[2])
    ro = run(oport)
    rf = run(fport)
    fails = 0
    leaks = 0
    for (lbl, a), (_, b) in zip(ro, rf):
        # (2) internal-namespace leak check on fr's reply
        for sent in SENTINELS:
            if sent in b:
                leaks += 1
                print("LEAK | %s — fr reply contains %r: %r" % (lbl, sent, b[:200]))
                break
        # (1) parity
        if lbl in ORDER_INSENSITIVE:
            ok = sorted(a.split(b"\r\n")) == sorted(b.split(b"\r\n"))
        else:
            ok = a == b
        if not ok:
            fails += 1
            print("FAIL | %s" % lbl)
            print("   oracle=%r" % a[:220])
            print("   fr    =%r" % b[:220])
    n = len(CASES)
    if fails or leaks:
        print("\n%d/%d match, %d namespace leak(s)  <-- FAIL" % (n - fails, n, leaks))
        sys.exit(1)
    print("OK: %d/%d non-zero-DB error/key cases byte-exact vs redis 7.2.4, no internal-namespace leak" % (n, n))
    sys.exit(0)


if __name__ == "__main__":
    main()
