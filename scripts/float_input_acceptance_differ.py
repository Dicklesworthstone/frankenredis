#!/usr/bin/env python3
"""Differential gate: float-input acceptance (frankenredis-cyj6i).

ZADD/ZINCRBY (string2d) and INCRBYFLOAT parse a float argument; exactly which string
forms are ACCEPTED vs rejected is a classic parity edge: scientific (1.5e3/E3),
leading '+', '.5'/'5.', all the infinity spellings (inf/+inf/-inf/Inf/INF/infinity),
C99 hex-float (0x1p4), 1e400 -> inf — all valid; nan/NaN, trailing junk (1.5x),
empty, lone 'e5', double-dot '1.5.5', comma/space-separated -> error. This gate pins
both the accept/reject decision (the reply) AND the resulting stored/formatted score
byte-exact vs redis 7.2.4. Distinct from float_format (output formatting),
hexfloat_incr (hex only), and zset_score_range (the strtod range-bound parser).

Usage: float_input_acceptance_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

SCORES = [
    "1.5", "1.5e3", "1.5E3", "1e10", "-2.5", "+3.5", ".5", "5.", "0.0", "-0.0",
    "inf", "+inf", "-inf", "Inf", "INF", "infinity", "+infinity", "-infinity",
    "1.5e", "e5", "1.5.5", "1,5", "1 5", " 1.5", "1.5 ", "1.5x", "x1.5", "", "   ",
    "nan", "NaN", "-nan", "0x1p4", "0x1.8p1", "1_000", "3.14159265358979",
    "1e400", "-1e400",
]


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.015)
    return s.recv(1 << 20)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
    fails = []

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    for sc in SCORES:
        for s in (od, fr):
            cmd(s, "DEL", "z")
        chk(f"zadd[{sc!r}]", "ZADD", "z", sc, "m")
        chk(f"zscore[{sc!r}]", "ZSCORE", "z", "m")
    for sc in ["1.5", "1e3", "inf", "nan", "1.5x", "", "0x1p4", "+inf", "-inf"]:
        for s in (od, fr):
            cmd(s, "DEL", "z2")
            cmd(s, "ZADD", "z2", "1", "m")
        chk(f"zincrby[{sc!r}]", "ZINCRBY", "z2", sc, "m")
        for s in (od, fr):
            cmd(s, "DEL", "fl")
            cmd(s, "SET", "fl", "1")
        chk(f"incrbyfloat[{sc!r}]", "INCRBYFLOAT", "fl", sc)

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} float-input divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — float-input acceptance byte-exact vs redis 7.2.4 "
        f"({len(SCORES)} ZADD forms x2 + ZINCRBY/INCRBYFLOAT: accept/reject + stored score)"
    )


if __name__ == "__main__":
    main()
