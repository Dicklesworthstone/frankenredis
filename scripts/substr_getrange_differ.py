#!/usr/bin/env python3
"""Differential gate: SUBSTR/GETRANGE alias-identity + degenerate ranges (frankenredis-6583h).

SUBSTR is the deprecated alias of GETRANGE and must behave byte-identically. Tests
normally use GETRANGE, so SUBSTR's alias-parity is only fuzzed, never pinned
deterministically. This gate, for a battery of index ranges (incl. negative,
out-of-range, start>end, start==len, both-negative-beyond-len), asserts: fr GETRANGE
== redis GETRANGE, fr SUBSTR == redis SUBSTR, AND fr SUBSTR == fr GETRANGE (the
alias is identical). Plus degenerate keys: empty string, missing key, wrong type,
non-integer index, arity.

Usage: substr_getrange_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

RANGES = [
    ("0", "4"), ("0", "-1"), ("-5", "-1"), ("6", "-1"), ("0", "0"), ("-1", "-1"),
    ("100", "200"), ("5", "2"), ("-100", "-50"), ("-100", "5"), ("0", "100"),
    ("11", "11"), ("10", "10"), ("-11", "-1"), ("-12", "-1"),
]


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.02)
    return s.recv(1 << 20)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "k", "Hello World")  # len 11
        cmd(s, "SET", "e", "")
        cmd(s, "RPUSH", "lst", "x")
    fails = []
    for st, en in RANGES:
        og, fg = cmd(od, "GETRANGE", "k", st, en), cmd(fr, "GETRANGE", "k", st, en)
        os_, fs = cmd(od, "SUBSTR", "k", st, en), cmd(fr, "SUBSTR", "k", st, en)
        if og != fg:
            fails.append(f"GETRANGE({st},{en}): redis={og!r} fr={fg!r}")
        if os_ != fs:
            fails.append(f"SUBSTR({st},{en}): redis={os_!r} fr={fs!r}")
        if fg != fs:
            fails.append(f"alias-identity({st},{en}): fr GETRANGE={fg!r} != fr SUBSTR={fs!r}")

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    chk("substr_empty", "SUBSTR", "e", "0", "-1")
    chk("substr_empty_0_0", "SUBSTR", "e", "0", "0")
    chk("substr_missing", "SUBSTR", "nope", "0", "-1")
    chk("substr_wrongtype", "SUBSTR", "lst", "0", "-1")
    chk("substr_badidx", "SUBSTR", "k", "x", "y")
    chk("substr_arity", "SUBSTR", "k", "0")
    chk("getrange_empty", "GETRANGE", "e", "0", "-1")
    chk("getrange_missing", "GETRANGE", "nope", "0", "5")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} SUBSTR/GETRANGE divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — SUBSTR/GETRANGE alias-identity + degenerate ranges byte-exact vs redis 7.2.4 "
        f"({len(RANGES)} ranges x3 + 8 edge cases)"
    )


if __name__ == "__main__":
    main()
