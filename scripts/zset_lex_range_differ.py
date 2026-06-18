#!/usr/bin/env python3
"""Differential gate: zset lexicographic range parsing (frankenredis-zavq6).

ZRANGEBYLEX / ZREVRANGEBYLEX / ZLEXCOUNT / ZRANGE ... BYLEX parse lex bounds with a
mandatory prefix byte: `[m` (inclusive), `(m` (exclusive), `+` (max), `-` (min). A
bare bound, `+x`, or an empty string is a "not valid string range" error. Member
order at equal score is the byte order of the members. This surface (bound parsing,
malformed-bound errors, reversed/empty ranges, LIMIT, REV) had no dedicated gate.
Compares every form byte-for-byte vs vendored redis 7.2.4.

Usage: zset_lex_range_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

# all members share score 0 so ordering is purely lexicographic; includes the
# empty-string member and a shared-prefix pair (b / ba).
MEMBERS = ["a", "b", "c", "d", "e", "f", "ba", ""]

CASES = [
    ["ZRANGEBYLEX", "z", "-", "+"],
    ["ZRANGEBYLEX", "z", "[b", "[d"],
    ["ZRANGEBYLEX", "z", "(b", "(d"],
    ["ZRANGEBYLEX", "z", "[b", "(d"],
    ["ZRANGEBYLEX", "z", "[c", "+"],
    ["ZRANGEBYLEX", "z", "-", "[c"],
    ["ZRANGEBYLEX", "z", "(a", "+"],
    ["ZRANGEBYLEX", "z", "[", "+"],          # bound "[" => include the empty member
    ["ZRANGEBYLEX", "z", "[b", "[bz"],       # shared-prefix range (b, ba)
    ["ZRANGEBYLEX", "z", "[d", "[b"],        # reversed => empty
    ["ZRANGEBYLEX", "z", "+", "-"],          # reversed => empty
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "2", "3"],
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "2", "-1"],
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "100", "5"],
    ["ZRANGEBYLEX", "z", "b", "d"],          # ERR not valid string range (no prefix)
    ["ZRANGEBYLEX", "z", "+x", "-"],         # ERR
    ["ZRANGEBYLEX", "z", "", "+"],           # ERR (empty bound)
    ["ZREVRANGEBYLEX", "z", "+", "-"],
    ["ZREVRANGEBYLEX", "z", "[d", "[b"],
    ["ZREVRANGEBYLEX", "z", "(d", "(b"],
    ["ZREVRANGEBYLEX", "z", "[b", "[d"],     # reversed => empty
    ["ZREVRANGEBYLEX", "z", "-", "+", "LIMIT", "0", "3"],
    ["ZLEXCOUNT", "z", "-", "+"],
    ["ZLEXCOUNT", "z", "[b", "[d"],
    ["ZLEXCOUNT", "z", "(a", "(c"],
    ["ZLEXCOUNT", "z", "x", "y"],            # ERR
    ["ZRANGE", "z", "[b", "[d", "BYLEX"],
    ["ZRANGE", "z", "[d", "[b", "BYLEX", "REV"],
    ["ZRANGE", "z", "-", "+", "BYLEX", "LIMIT", "1", "2"],
    ["ZRANGEBYLEX", "nope", "-", "+"],       # missing key => empty
    ["ZLEXCOUNT", "nope", "-", "+"],
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
        args = ["ZADD", "z"]
        for m in MEMBERS:
            args += ["0", m]
        cmd(s, *args)
    fails = []
    for argv in CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{' '.join(argv)!r}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} zset lex-range divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — zset lexicographic range byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases: ZRANGEBYLEX/ZREVRANGEBYLEX/ZLEXCOUNT/ZRANGE BYLEX, "
        "bounds/errors/LIMIT/REV)"
    )


if __name__ == "__main__":
    main()
