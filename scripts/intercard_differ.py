#!/usr/bin/env python3
"""Differential gate: SINTERCARD / ZINTERCARD (frankenredis-yzyho).

Both take `numkeys key... [LIMIT n]` and share fiddly validation: numkeys must be a
positive integer and must not exceed the number of key args; LIMIT must be a
non-negative integer where 0 means "unlimited"; a missing key makes the
intersection empty; a wrong-type operand errors. Pins all of it byte-exact vs
redis 7.2.4.

Usage: intercard_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time


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


CASES = [
    ["SINTERCARD", "2", "s1", "s2"],
    ["SINTERCARD", "3", "s1", "s2", "s3"],
    ["SINTERCARD", "2", "s1", "s2", "LIMIT", "2"],
    ["SINTERCARD", "2", "s1", "s2", "LIMIT", "0"],          # 0 = unlimited
    ["SINTERCARD", "2", "s1", "s2", "LIMIT", "100"],
    ["SINTERCARD", "2", "s1", "s2", "LIMIT", "-1"],         # err: negative
    ["SINTERCARD", "0", "s1"],                              # err: numkeys <= 0
    ["SINTERCARD", "-1", "s1"],                             # err
    ["SINTERCARD", "abc", "s1"],                            # err: not int
    ["SINTERCARD", "3", "s1", "s2"],                        # err: numkeys > args
    ["SINTERCARD", "2", "s1", "nope"],                      # 0: intersect empty
    ["SINTERCARD", "2", "s1", "str"],                       # WRONGTYPE
    ["SINTERCARD", "2", "s1", "s2", "LIMIT", "x"],          # err: LIMIT not int
    ["SINTERCARD", "2", "s1", "s2", "FOO", "2"],            # err: syntax
    ["SINTERCARD", "1", "s1"],
    ["ZINTERCARD", "2", "z1", "z2"],
    ["ZINTERCARD", "2", "z1", "z2", "LIMIT", "2"],
    ["ZINTERCARD", "2", "z1", "z2", "LIMIT", "0"],
    ["ZINTERCARD", "2", "z1", "z2", "LIMIT", "-1"],         # err
    ["ZINTERCARD", "0", "z1"],                              # err
    ["ZINTERCARD", "2", "z1", "str"],                       # WRONGTYPE
    ["ZINTERCARD", "2", "z1", "nope"],                      # 0
    ["ZINTERCARD", "2", "z1", "s1"],                        # cross-type zset ∩ set
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "SADD", "s1", "a", "b", "c", "d")
        cmd(s, "SADD", "s2", "b", "c", "d", "e")
        cmd(s, "SADD", "s3", "c", "d", "f")
        cmd(s, "ZADD", "z1", "1", "a", "2", "b", "3", "c", "4", "d")
        cmd(s, "ZADD", "z2", "1", "b", "2", "c", "3", "d", "4", "e")
        cmd(s, "SET", "str", "x")
    fails = []
    for argv in CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{' '.join(argv)!r}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} SINTERCARD/ZINTERCARD divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — SINTERCARD/ZINTERCARD byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases: numkeys/LIMIT validation, missing-key, wrongtype, cross-type)"
    )


if __name__ == "__main__":
    main()
