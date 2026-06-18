#!/usr/bin/env python3
"""Differential gate for RESP3 reply TYPES (HELLO 3) across collection commands.

In RESP3 (HELLO 3) many commands switch reply shape: hashes/configs become maps
(`%`), set replies become sets (`~`), scores/floats become doubles (`,`), missing
doubles become `,nan`/null, etc. Emitting the RESP2 shape (or the wrong RESP3
marker) under HELLO 3 is a subtle, easy-to-regress parity bug — fr has shipped
two such before (ZRANK WITHSCORE / BZPOPMIN emitted bulk strings instead of RESP3
doubles). algebra_resp3_differ.py covers the arithmetic/aggregation family; this
pins the reply-TYPE markers + content of the collection/introspection commands
that change shape in RESP3, byte-for-byte against vendored redis 7.2.4.

Only DETERMINISTIC commands are probed (no SPOP/SRANDMEMBER/random sampling, no
connection- or config-dependent introspection), so every reply is reproducible
across the two processes.

Usage: resp3_reply_type_gate.py <oracle_port> <fr_port>   (oracle = vendored redis)
"""
import socket
import sys


def _read_reply(s):
    data = bytearray()

    def read_line():
        line = bytearray()
        while not line.endswith(b"\r\n"):
            ch = s.recv(1)
            if not ch:
                break
            line += ch
        return bytes(line)

    def one():
        line = read_line()
        data.extend(line)
        if not line:
            return
        t = line[:1]
        if t in (b"+", b"-", b":", b"_", b"#", b",", b"("):
            return
        if t in (b"$", b"=", b"!"):
            n = int(line[1:-2])
            if n < 0:
                return
            body = b""
            while len(body) < n + 2:
                body += s.recv(n + 2 - len(body))
            data.extend(body)
            return
        if t in (b"*", b"~", b">", b"%"):
            n = int(line[1:-2])
            if n < 0:
                return
            if t == b"%":
                n *= 2
            for _ in range(n):
                one()

    one()
    return bytes(data)


def send(s, *args):
    buf = b"*%d\r\n" % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        buf += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(buf)
    return _read_reply(s)


def setup(s):
    send(s, "HELLO", "3")
    send(s, "FLUSHALL")
    send(s, "HSET", "h", "f1", "v1", "f2", "v2", "f3", "v3")
    send(s, "SADD", "s", "alpha", "beta", "gamma")
    send(s, "ZADD", "z", "1.5", "m1", "2.5", "m2", "inf", "m3", "-inf", "m4", "3.14159", "m5")
    send(s, "RPUSH", "l", "x", "y", "z")
    send(s, "SET", "str", "hello world")
    send(s, "SET", "num", "42")
    send(s, "XADD", "st", "1-1", "a", "1")
    send(s, "XADD", "st", "2-1", "b", "2")
    send(s, "PFADD", "hll", "x", "y", "z")
    send(s, "GEOADD", "geo", "13.361389", "38.115556", "P1", "15.087269", "37.502669", "P2")
    # A consumer group with no reads -> deterministic XINFO GROUPS / empty XPENDING
    # summary (lag/entries-read/last-delivered-id are fixed; nothing pending so no
    # idle/inactive timing fields are emitted, unlike XINFO CONSUMERS).
    send(s, "XGROUP", "CREATE", "st", "g1", "0")


# Deterministic, RESP3-shape-sensitive probes (maps %, sets ~, doubles ,, nulls).
PROBES = [
    ["HGETALL", "h"],
    ["SMEMBERS", "s"],
    ["SINTER", "s"],
    ["SUNION", "s"],
    ["SDIFF", "s"],
    ["SPOP", "s", "0"],                         # count 0 -> empty set, deterministic
    ["ZSCORE", "z", "m1"],
    ["ZSCORE", "z", "m3"],
    ["ZSCORE", "z", "m4"],
    ["ZSCORE", "z", "missing"],
    ["ZMSCORE", "z", "m1", "missing", "m5"],
    ["ZRANGE", "z", "0", "-1", "WITHSCORES"],
    ["ZRANGEBYSCORE", "z", "-inf", "+inf", "WITHSCORES"],
    ["ZPOPMIN", "z", "0"],                      # count 0 -> empty, deterministic
    ["ZRANK", "z", "m2", "WITHSCORE"],
    ["ZRANK", "z", "missing", "WITHSCORE"],
    ["GEOPOS", "geo", "P1", "missing"],
    ["GEODIST", "geo", "P1", "P2"],
    ["GEODIST", "geo", "P1", "missing"],
    ["XRANGE", "st", "-", "+"],
    ["INCRBYFLOAT", "newf", "3.14"],
    ["HINCRBYFLOAT", "h", "nf", "2.5"],
    ["PFCOUNT", "hll"],
    ["TYPE", "z"],
    ["OBJECT", "ENCODING", "z"],
    ["EXISTS", "h", "s", "z", "nope"],
    ["LMPOP", "1", "l", "LEFT", "COUNT", "2"],
    ["SINTERCARD", "1", "s"],
    ["DEBUG", "DIGEST-VALUE", "z"],
    # Complex NESTED RESP3 maps (highest regression risk — a wrong inner type
    # marker is easy to miss). All fields here are deterministic: fixed XADD ids,
    # a freshly-created group with no reads, and static command/function metadata.
    ["XINFO", "STREAM", "st"],
    ["XINFO", "GROUPS", "st"],
    ["XPENDING", "st", "g1"],
    ["COMMAND", "INFO", "get"],
    ["COMMAND", "DOCS", "GET"],
    ["FUNCTION", "STATS"],
    ["FUNCTION", "LIST"],
]


def run(oracle, fr):
    setup(oracle)
    setup(fr)
    diffs = 0
    for p in PROBES:
        ro, rf = send(oracle, *p), send(fr, *p)
        if ro != rf:
            diffs += 1
            print(f"DIFF [{' '.join(p)}]\n  redis={ro!r}\n  fr   ={rf!r}")
    if diffs == 0:
        print(f"PASS — RESP3 reply types byte-exact vs redis 7.2.4 ({len(PROBES)} probes)")
    else:
        print(f"FAIL — {diffs} divergence(s)")
    return 1 if diffs else 0


def main():
    if len(sys.argv) < 3:
        print("usage: resp3_reply_type_gate.py <oracle_port> <fr_port>", file=sys.stderr)
        return 2
    oracle = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=10)
    fr = socket.create_connection(("127.0.0.1", int(sys.argv[2])), timeout=10)
    oracle.settimeout(10)
    fr.settimeout(10)
    try:
        return run(oracle, fr)
    finally:
        oracle.close()
        fr.close()


if __name__ == "__main__":
    sys.exit(main())
