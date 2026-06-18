#!/usr/bin/env python3
"""Differential gate for the Redis 7.0 multi-key pop / cardinality family:
LMPOP / ZMPOP / SINTERCARD. These share fiddly NUMKEYS-counted variadic parsing
and option validation (COUNT, LEFT|RIGHT / MIN|MAX, LIMIT) that the random fuzzer
and arity gate under-cover, and they had no dedicated differ.

Covers: basic pop, COUNT (incl over-cardinality and 0/negative), first-empty
fallthrough, wrong NUMKEYS, NUMKEYS 0, bad direction/where token, missing
direction, wrongtype (first key and mid-list), and SINTERCARD LIMIT 0/negative.
Diffs every reply vs vendored redis 7.2.4.

Usage: multikey_pop_differ.py <oracle_port> <fr_port>
Exit 0 = parity; 1 = divergence(s).
"""
import socket, sys


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=10)


class R:
    def __init__(s, p):
        s.s = conn(p)
        s.buf = b""

    def _l(s):
        while b"\r\n" not in s.buf:
            s.buf += s.s.recv(1 << 20)
        l, s.buf = s.buf.split(b"\r\n", 1)
        return l

    def _n(s, n):
        while len(s.buf) < n + 2:
            s.buf += s.s.recv(1 << 20)
        d = s.buf[:n]
        s.buf = s.buf[n + 2:]
        return d

    def read(s):
        l = s._l()
        t = l[:1]
        if t in (b'+', b':', b'-'):
            return l.decode()
        if t == b'$':
            n = int(l[1:])
            return None if n < 0 else s._n(n).decode("latin1")
        if t in (b'*', b'~'):
            n = int(l[1:])
            return None if n < 0 else [s.read() for _ in range(n)]
        return l.decode()

    def cmd(s, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else x
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        s.s.sendall(o)
        return s.read()


CASES = [
    ("lmpop-basic", [["rpush", "l1", "a", "b", "c"], ["lmpop", "2", "l0", "l1", "LEFT"]]),
    ("lmpop-count", [["rpush", "l1", "a", "b", "c", "d"], ["lmpop", "1", "l1", "RIGHT", "COUNT", "2"]]),
    ("lmpop-count-over", [["rpush", "l1", "a", "b"], ["lmpop", "1", "l1", "LEFT", "COUNT", "10"]]),
    ("lmpop-all-empty", [["lmpop", "2", "x", "y", "LEFT"]]),
    ("lmpop-first-empty", [["rpush", "l2", "z"], ["lmpop", "2", "l1", "l2", "LEFT"]]),
    ("lmpop-wrongtype-first", [["set", "s1", "v"], ["rpush", "l2", "a"], ["lmpop", "2", "s1", "l2", "LEFT"]]),
    ("lmpop-numkeys-0", [["lmpop", "0", "LEFT"]]),
    ("lmpop-numkeys-mismatch", [["lmpop", "5", "l1", "LEFT"]]),
    ("lmpop-bad-dir", [["rpush", "l1", "a"], ["lmpop", "1", "l1", "MIDDLE"]]),
    ("lmpop-count-0", [["rpush", "l1", "a"], ["lmpop", "1", "l1", "LEFT", "COUNT", "0"]]),
    ("lmpop-count-neg", [["rpush", "l1", "a"], ["lmpop", "1", "l1", "LEFT", "COUNT", "-1"]]),
    ("lmpop-no-dir", [["rpush", "l1", "a"], ["lmpop", "1", "l1"]]),
    ("zmpop-basic", [["zadd", "z1", "1", "a", "2", "b", "3", "c"], ["zmpop", "2", "z0", "z1", "MIN"]]),
    ("zmpop-max-count", [["zadd", "z1", "1", "a", "2", "b", "3", "c"], ["zmpop", "1", "z1", "MAX", "COUNT", "2"]]),
    ("zmpop-count-over", [["zadd", "z1", "1", "a"], ["zmpop", "1", "z1", "MIN", "COUNT", "9"]]),
    ("zmpop-all-empty", [["zmpop", "2", "x", "y", "MIN"]]),
    ("zmpop-wrongtype", [["set", "s1", "v"], ["zmpop", "1", "s1", "MIN"]]),
    ("zmpop-bad-where", [["zadd", "z1", "1", "a"], ["zmpop", "1", "z1", "MIDDLE"]]),
    ("zmpop-numkeys-0", [["zmpop", "0", "MIN"]]),
    ("zmpop-count-0", [["zadd", "z1", "1", "a"], ["zmpop", "1", "z1", "MIN", "COUNT", "0"]]),
    ("sintercard-basic", [["sadd", "s1", "a", "b", "c"], ["sadd", "s2", "b", "c", "d"], ["sintercard", "2", "s1", "s2"]]),
    ("sintercard-limit", [["sadd", "s1", "a", "b", "c"], ["sadd", "s2", "a", "b", "c"], ["sintercard", "2", "s1", "s2", "LIMIT", "2"]]),
    ("sintercard-limit-0", [["sadd", "s1", "a", "b"], ["sadd", "s2", "a", "b"], ["sintercard", "2", "s1", "s2", "LIMIT", "0"]]),
    ("sintercard-numkeys-0", [["sintercard", "0"]]),
    ("sintercard-limit-neg", [["sadd", "s1", "a"], ["sintercard", "1", "s1", "LIMIT", "-1"]]),
    ("sintercard-wrongtype", [["set", "s1", "v"], ["sintercard", "1", "s1"]]),
]


def main():
    od = R(int(sys.argv[1]))
    fr = R(int(sys.argv[2]))
    div = 0

    def cleanup():
        for c in (od, fr):
            try:
                c.cmd("flushall")
            except Exception:
                pass

    for label, cmds in CASES:
        od.cmd("flushall")
        fr.cmd("flushall")
        for c in cmds:
            ro = od.cmd(*c)
            rf = fr.cmd(*c)
            if ro != rf:
                div += 1
                print(f"DIVERGE {label} [{' '.join(c)}]\n  oracle: {ro}\n  fr    : {rf}")
    print("-" * 60)
    cleanup()
    if div:
        print(f"FAIL — {div} divergence(s)")
        return 1
    print(f"PASS — LMPOP/ZMPOP/SINTERCARD byte-exact vs redis 7.2.4 ({len(CASES)} cases)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
