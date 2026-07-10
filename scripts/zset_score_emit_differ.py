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

ALSO pins the score's *persistence entry encoding* (DUMP payload bytes), which the reply
emitters above do not exercise. That was a real bug: fr's RDB-save encoder rendered scores
with Rust's `{}` (never scientific) instead of d2string, writing "0.0000001" for 1e-7,
a 301-digit decimal for 1.5e300, and an int64 entry for 5e18 where upstream stores "5e+18".
The EDGE_SCORES table walks d2string's actual branch points:
  * ll2string window is +-(LLONG_MAX/2) == 2^62, NOT 2^52: 1e16/1e18/2^62 print plain,
    5e18 prints "5e+18";
  * above the window grisu2 STILL plain-renders some integral doubles, e.g.
    6917529027641081856 -> "6917529027641082000", which re-parses and int-encodes;
  * grisu2's plain/scientific flip: 1e-5 -> "0.00001" but 1e-7 -> "1e-7";
  * 17-significant-digit round trips, subnormals, and nan rejection.

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

# Scores whose d2string rendering sits on a branch boundary. Each is DUMPed in its own
# single-member zset so the listpack score entry is isolated and compared byte-for-byte.
EDGE_SCORES = [
    "0",
    "-0",
    "1",
    "4503599627370496",  # 2^52 (NOT the ll2string bound, despite folklore)
    "4503599627370497",
    "9007199254740992",  # 2^53
    "1000000000000000",  # 1e15
    "10000000000000000",  # 1e16 -> plain "10000000000000000"
    "1000000000000000000",  # 1e18 -> plain
    "2000000000000000000",
    "4000000000000000000",
    "4611686018427387904",  # 2^62 == double2ll's inclusive bound -> plain, int entry
    "4611686018427387905",  # rounds to 2^62
    "6917529027641081856",  # > 2^62 yet plain-renders -> STILL an int entry upstream
    "9223372036854775808",  # 2^63 -> plain render, but overflows i64 -> string entry
    "5000000000000000000",  # -> "5e+18" -> string entry
    "7000000000000000000",  # -> "7e+18"
    "100000000000000000000",  # 1e20 -> "1e+20"
    "0.00001",  # 1e-5 stays fixed-point
    "0.000001",  # 1e-6 stays fixed-point
    "1e-7",  # flips to scientific
    "1e-10",
    "1e-100",
    # Rendered plain (Rust `{}` style) these exceed zzlStrtod's 128-byte buffer, and a real
    # redis loading such an RDB silently truncates: 1.5e300 -> 1.5e+126, 1e-200 -> 0.
    "1e-200",
    "1e200",
    "0.30000000000000004",  # 0.1 + 0.2, 17 sig digits
    "1.2345678901234567",
    "123456789.123456789",  # -> "1.2345678912345679e+8" (scientific at ~1.2e8)
    "2.2250738585072014e-308",  # smallest normal
    "2.225073858507201e-308",  # largest subnormal
    "5e-324",  # smallest subnormal
    "-5e-324",
    "1.7976931348623157e308",  # f64::MAX
    "1.5e300",
    "-1.5e300",
    "inf",
    "-inf",
]

# Both engines must reject these outright (never stored, never rendered).
REJECTED_SCORES = ["nan", "-nan", "+nan", "NaN"]


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

    # Persistence entry encoding: one member per key so each score's listpack entry is
    # isolated. ZSCORE guards the render; DUMP guards how that render is *stored*.
    for i, score in enumerate(EDGE_SCORES):
        key = f"zedge{i}"
        for s in (o, f):
            cmd(s, "DEL", key)
            cmd(s, "ZADD", key, score, "m")
        chk(f"edge_zscore[{score}]", "ZSCORE", key, "m")
        chk(f"edge_dump[{score}]", "DUMP", key)
        chk(f"edge_encoding[{score}]", "OBJECT", "ENCODING", key)

    # nan must be rejected identically (it can never become a stored score).
    for score in REJECTED_SCORES:
        for s in (o, f):
            cmd(s, "DEL", "znan")
        chk(f"nan_rejected[{score}]", "ZADD", "znan", score, "m")

    if fails:
        print(f"FAIL — {len(fails)} zset score-emit divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — zset score-output formatting byte-exact vs redis 7.2.4 "
        "(ZSCORE/ZMSCORE/ZINCRBY/ZADD-INCR/WITHSCORES/ZPOPMIN-MAX x int/float/inf/2^53/"
        "max-f64/subnormal x RESP2+RESP3 Double) [guards n2u1g direct-encode]; "
        f"plus {len(EDGE_SCORES)} d2string branch-boundary scores pinned through "
        f"ZSCORE + DUMP + OBJECT ENCODING, and {len(REJECTED_SCORES)} nan forms rejected "
        "[guards the RDB-save d2string fix]"
    )


if __name__ == "__main__":
    main()
