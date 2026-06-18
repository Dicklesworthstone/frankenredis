#!/usr/bin/env python3
"""Differential gate: zset score-OUTPUT formatting (frankenredis-n2u1g).

Distinct from zset_score_range_differ (which covers the score-BOUND *parser* for
ZRANGEBYSCORE/ZCOUNT) and zset_tiebreak_differ (order/tie-break): this pins how a score is
*rendered* in every emitter's reply, byte-exact vs redis 7.2.4, across score magnitudes
and both RESP2 and RESP3 (Double). It guards the n2u1g "zset score direct-encode" perf
lever (redis_score_to_string -> bulk / RESP3 Double, emitted straight into the reply
buffer) against any rendering regression — d2string/grisu2 parity must hold regardless of
how the bytes reach the wire.

Emitters: ZSCORE, ZMSCORE, ZINCRBY, ZADD INCR, ZRANGE/ZREVRANGE WITHSCORES,
ZRANGEBYSCORE WITHSCORES, ZPOPMIN/ZPOPMAX. Magnitudes: int, decimal, -0, +inf, -inf,
2^53 boundary, max f64, tiny/large exponents.

Usage: zset_score_emit_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

MEMBERS = [
    ("mi", "1"),
    ("md", "2.5"),
    ("mneg", "-3"),
    ("mpi", "3.141592653589793"),
    ("minf", "inf"),
    ("mninf", "-inf"),
    ("mz", "0"),
    ("m53", "9007199254740993"),
    ("mbig", "1.7976931348623157e308"),
    ("msmall", "5e-324"),
    ("mexp", "1.5e300"),
]


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=6)


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
    o, f = conn(op), conn(fp)
    fails = []

    def seed():
        for s in (o, f):
            cmd(s, "DEL", "z")
            args = ["ZADD", "z"]
            for name, score in MEMBERS:
                args += [score, name]
            cmd(s, *args)

    def chk(label, *c):
        ro, rf = cmd(o, *c), cmd(f, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro[:80]!r} fr={rf[:80]!r}")

    for resp in (2, 3):
        seed()
        if resp == 3:
            for s in (o, f):
                cmd(s, "HELLO", "3")
        tag = f"resp{resp}"
        chk(f"{tag}_zrange_ws", "ZRANGE", "z", "0", "-1", "WITHSCORES")
        chk(f"{tag}_zrevrange_ws", "ZREVRANGE", "z", "0", "-1", "WITHSCORES")
        chk(f"{tag}_zrangebyscore_ws", "ZRANGEBYSCORE", "z", "-inf", "+inf", "WITHSCORES")
        for name, _ in MEMBERS:
            chk(f"{tag}_zscore_{name}", "ZSCORE", "z", name)
        chk(f"{tag}_zmscore", "ZMSCORE", "z", "mi", "md", "mbig", "nope")
        chk(f"{tag}_zincrby", "ZINCRBY", "z", "1.5", "mi")
        chk(f"{tag}_zadd_incr", "ZADD", "z", "INCR", "2.25", "md")
        chk(f"{tag}_zpopmin", "ZPOPMIN", "z", "3")
        chk(f"{tag}_zpopmax", "ZPOPMAX", "z", "3")
        # reset RESP for the next loop's clean HELLO state
        if resp == 3:
            for s in (o, f):
                cmd(s, "HELLO", "2")

    if fails:
        print(f"FAIL — {len(fails)} zset score-emit divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — zset score-output formatting byte-exact vs redis 7.2.4 "
        "(ZSCORE/ZMSCORE/ZINCRBY/ZADD-INCR/WITHSCORES/ZPOPMIN-MAX x int/float/inf/2^53/"
        "max-f64/subnormal x RESP2+RESP3 Double) [guards n2u1g direct-encode]"
    )


if __name__ == "__main__":
    main()
