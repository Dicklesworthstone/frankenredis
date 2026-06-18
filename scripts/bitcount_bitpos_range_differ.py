#!/usr/bin/env python3
"""Differential gate: BITCOUNT / BITPOS range + BIT|BYTE indexing (frankenredis-1ftub).

The no-range BITCOUNT/BITPOS forms have byte-prefix fast paths (covered by
packet_fastpath_differ). The RANGE forms go through the generic path and carry the
subtle bits: signed start/end byte indices, negative indices, out-of-bounds
clamping, reversed ranges (start>end => 0 / -1), the redis 7.0 BIT|BYTE unit
selector, bad-unit errors, and BITPOS's special "first 0-bit past the end" rule
(an all-ones string: `BITPOS k 0` with NO range returns len*8, but WITH a range
returns -1). This gate pins all of that byte-for-byte vs vendored redis 7.2.4.

Usage: bitcount_bitpos_range_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

# (key, raw-byte value) set up before the cases run.
VALUES = {
    "k": "foobar",
    "ff": "\xff\xf0\x0f",
    "allff": "\xff\xff",
    "zero": "\x00\x00\x00",
}

CASES = [
    # BITCOUNT byte-range (BYTE is the default unit)
    ["BITCOUNT", "k"],
    ["BITCOUNT", "k", "0", "0"],
    ["BITCOUNT", "k", "1", "1"],
    ["BITCOUNT", "k", "0", "-1"],
    ["BITCOUNT", "k", "-2", "-1"],
    ["BITCOUNT", "k", "0", "100"],
    ["BITCOUNT", "k", "5", "1"],            # start>end => 0
    ["BITCOUNT", "nope", "0", "-1"],
    # BITCOUNT BIT indexing (redis 7.0)
    ["BITCOUNT", "k", "0", "0", "BIT"],
    ["BITCOUNT", "k", "0", "5", "BIT"],
    ["BITCOUNT", "k", "5", "30", "BIT"],
    ["BITCOUNT", "k", "-8", "-1", "BIT"],
    ["BITCOUNT", "k", "0", "0", "BYTE"],
    ["BITCOUNT", "k", "0", "1000", "BIT"],
    ["BITCOUNT", "k", "0", "0", "NIBBLE"],  # ERR syntax (bad unit)
    # BITPOS byte-range
    ["BITPOS", "ff", "0"],
    ["BITPOS", "ff", "1"],
    ["BITPOS", "ff", "0", "2"],
    ["BITPOS", "ff", "1", "0", "-1"],
    ["BITPOS", "ff", "0", "0"],
    ["BITPOS", "zero", "1"],                # no 1-bit => -1
    ["BITPOS", "zero", "0"],                # 0
    ["BITPOS", "nope", "1"],
    ["BITPOS", "nope", "0"],
    # BITPOS BIT indexing
    ["BITPOS", "ff", "0", "0", "-1", "BIT"],
    ["BITPOS", "ff", "1", "8", "23", "BIT"],
    ["BITPOS", "ff", "0", "1", "2", "BYTE"],
    ["BITPOS", "k", "1", "0", "-1", "BIT"],
    ["BITPOS", "ff", "1", "0", "-1", "WORD"],  # ERR syntax (bad unit)
    # all-ones special case: no range => len*8 (first 0 past end); range given => -1
    ["BITPOS", "allff", "0"],
    ["BITPOS", "allff", "0", "0", "-1"],
    ["BITPOS", "allff", "0", "0", "-1", "BIT"],
    ["BITPOS", "allff", "1"],
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
        for k, v in VALUES.items():
            cmd(s, "SET", k, v)
    fails = []
    for argv in CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{' '.join(argv)!r}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} BITCOUNT/BITPOS range divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — BITCOUNT/BITPOS range + BIT/BYTE indexing byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases: neg indices/OOB/reversed/bad-unit/all-ones BITPOS-0 special case)"
    )


if __name__ == "__main__":
    main()
