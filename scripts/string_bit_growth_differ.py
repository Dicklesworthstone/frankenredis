#!/usr/bin/env python3
"""Differential gate: SETBIT/SETRANGE string-growth interaction chain (frankenredis-2ovmf).

SETBIT and SETRANGE grow a string and zero-pad the gap; the exact resulting bytes,
the STRLEN after growth (and that clearing a bit does NOT shrink it), GETBIT past
the end (0), APPEND onto a bit-grown value, the int->raw encoding conversion when a
bit/range write touches an integer-encoded key, and the SETRANGE-with-empty-value
no-op (which must NOT create a missing key) are all subtle interactions that the
single-command gates don't exercise as a chain. Pinned byte-exact vs redis 7.2.4.

Usage: string_bit_growth_differ.py <oracle_port> <fr_port>
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
    # SETBIT grows + zero-pads
    ["SETBIT", "b", "100", "1"], ["STRLEN", "b"], ["GET", "b"],
    ["GETBIT", "b", "100"], ["GETBIT", "b", "50"], ["GETBIT", "b", "9999"],
    ["BITCOUNT", "b"],
    ["SETBIT", "b", "100", "0"], ["BITCOUNT", "b"], ["STRLEN", "b"],  # clear keeps length
    # SETRANGE grows + zero-pads
    ["SETRANGE", "sr", "5", "AB"], ["STRLEN", "sr"], ["GET", "sr"],
    ["GETRANGE", "sr", "0", "-1"], ["GETRANGE", "sr", "4", "6"],
    # APPEND onto a bit-grown value
    ["APPEND", "b", "XY"], ["STRLEN", "b"], ["OBJECT", "ENCODING", "b"],
    # SETRANGE with empty value: no-op on existing, NO-create on missing
    ["SETRANGE", "sr", "0", ""], ["STRLEN", "sr"],
    ["SETRANGE", "nope", "0", ""], ["EXISTS", "nope"],
    ["GETRANGE", "nope2", "0", "-1"],
    # int-encoded key then bit/range write -> raw
    ["SET", "ik", "12345"],
    ["SETBIT", "ik", "0", "1"], ["OBJECT", "ENCODING", "ik"], ["GET", "ik"], ["BITCOUNT", "ik"],
    # large offset within the 4Gbit / 512MiB limit
    ["SETBIT", "huge", "8388607", "1"], ["STRLEN", "huge"],
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
    fails = []
    for argv in CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{' '.join(argv)!r}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} string-bit-growth divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — SETBIT/SETRANGE string-growth interaction byte-exact vs redis 7.2.4 "
        f"({len(CASES)} ops: zero-pad/STRLEN/GETBIT-past/APPEND/int->raw/empty-no-create/4Mb)"
    )


if __name__ == "__main__":
    main()
