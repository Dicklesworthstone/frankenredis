#!/usr/bin/env python3
"""Differential gate: zset score-bound parser (frankenredis-86deu).

ZRANGEBYSCORE / ZREVRANGEBYSCORE / ZCOUNT parse score bounds via the strtod path
(distinct from the lex-bound parser covered by zset_lex_range_differ): `(` =
exclusive, `+inf`/`-inf`/`inf`, plain/decimal numbers, C99 hex-float (yu071), and
malformed bounds error. ZREVRANGEBYSCORE takes (max min) order. This gate pins the
systematic inclusive/exclusive matrix + inf + hex-float + error cases + LIMIT +
negative scores byte-exact vs redis 7.2.4.

Usage: zset_score_range_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

# (lo, hi) bound pairs in normal (min,max) order; ZREVRANGEBYSCORE gets them swapped.
PAIRS = [
    ("-inf", "+inf"), ("1", "3"), ("(1", "3"), ("1", "(3"), ("(1", "(3"),
    ("(2", "(4"), ("-inf", "(3"), ("(3", "+inf"), ("inf", "-inf"),
    ("2.5", "3.5"), ("(2.5", "(3.5"), ("5", "1"), ("(5", "(1"),
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
        cmd(s, "ZADD", "z", "1", "a", "2", "b", "3", "c", "4", "d", "5", "e")
        cmd(s, "ZADD", "zneg", "-10", "x", "0", "y", "10", "z")
    fails = []

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    for lo, hi in PAIRS:
        chk(f"zrbs_{lo}_{hi}", "ZRANGEBYSCORE", "z", lo, hi)
        chk(f"zrbs_{lo}_{hi}_ws", "ZRANGEBYSCORE", "z", lo, hi, "WITHSCORES")
        chk(f"zrevbs_{hi}_{lo}", "ZREVRANGEBYSCORE", "z", hi, lo)
        chk(f"zcount_{lo}_{hi}", "ZCOUNT", "z", lo, hi)
    # LIMIT
    chk("limit", "ZRANGEBYSCORE", "z", "-inf", "+inf", "LIMIT", "1", "2")
    chk("limit_neg", "ZRANGEBYSCORE", "z", "-inf", "+inf", "LIMIT", "1", "-1")
    chk("limit_ws", "ZRANGEBYSCORE", "z", "(1", "+inf", "WITHSCORES", "LIMIT", "0", "2")
    # hex-float bounds (yu071)
    chk("hexfloat", "ZRANGEBYSCORE", "z", "0x1.0p1", "0x1.8p1")
    # malformed bound errors
    chk("bad_lo", "ZRANGEBYSCORE", "z", "notanum", "3")
    chk("bad_excl", "ZRANGEBYSCORE", "z", "(notanum", "3")
    chk("empty_lo", "ZRANGEBYSCORE", "z", "", "3")
    chk("nan", "ZRANGEBYSCORE", "z", "nan", "3")
    chk("plus_alone", "ZRANGEBYSCORE", "z", "+", "3")     # lex token, invalid as score
    chk("zcount_bad", "ZCOUNT", "z", "x", "y")
    # negative scores + missing key
    chk("neg_scores", "ZCOUNT", "zneg", "(-10", "10")
    chk("neg_range", "ZRANGEBYSCORE", "zneg", "-inf", "0")
    chk("missing_zrbs", "ZRANGEBYSCORE", "nope", "-inf", "+inf")
    chk("missing_zcount", "ZCOUNT", "nope", "-inf", "+inf")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} score-bound divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — zset score-bound parser byte-exact vs redis 7.2.4 "
        f"({len(PAIRS)} pairs x4 + LIMIT/hexfloat/errors/neg/missing)"
    )


if __name__ == "__main__":
    main()
