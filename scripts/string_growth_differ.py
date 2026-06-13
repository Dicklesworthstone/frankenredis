#!/usr/bin/env python3
"""Differential: string-growth surface (SETRANGE/SETBIT/GETRANGE/APPEND).

These commands have subtle edge semantics that hand-probes and the random fuzzer
under-cover: zero-fill on extend, the proto-max-bulk-len/512MB offset ceiling,
the bit-offset ceiling (2^32-1), the "bit is not 0 or 1" / "offset out of range"
error wording, and the int->raw OBJECT ENCODING transition a modify triggers.
This runs each as a short sequence and diffs every reply vs vendored redis 7.2.4.

Usage: string_growth_differ.py <oracle_port> <fr_port>
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
            return l
        if t == b'$':
            n = int(l[1:])
            return None if n < 0 else s._n(n)
        if t in (b'*', b'~'):
            n = int(l[1:])
            return None if n < 0 else [s.read() for _ in range(n)]
        return l

    def cmd(s, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else (
                str(x).encode() if not isinstance(x, bytes) else x)
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        s.s.sendall(o)
        return s.read()


SEQS = [
    ("setrange-missing", [["setrange", "k", "5", "hello"], ["get", "k"]]),
    ("setrange-empty-val", [["set", "k", "abc"], ["setrange", "k", "0", ""], ["get", "k"]]),
    ("setrange-extend", [["set", "k", "abc"], ["setrange", "k", "5", "XY"], ["get", "k"]]),
    ("setrange-neg-off", [["setrange", "k", "-1", "x"]]),
    ("setrange-huge-off", [["setrange", "k", "536870911", "x"]]),
    ("setrange-too-big", [["setrange", "k", "536870912", "x"]]),
    ("setbit-grow", [["setbit", "k", "100", "1"], ["strlen", "k"], ["getbit", "k", "100"]]),
    ("setbit-bad-bit", [["setbit", "k", "7", "2"]]),
    ("setbit-neg", [["setbit", "k", "-1", "1"]]),
    ("setbit-huge", [["setbit", "k", "4294967296", "1"]]),
    ("setbit-max", [["setbit", "k", "4294967295", "1"], ["strlen", "k"]]),
    ("getrange-neg", [["set", "k", "Hello World"], ["getrange", "k", "-5", "-1"]]),
    ("getrange-oob", [["set", "k", "abc"], ["getrange", "k", "10", "20"]]),
    ("getrange-inv", [["set", "k", "abc"], ["getrange", "k", "-1", "-5"]]),
    ("getrange-empty", [["getrange", "missing", "0", "-1"]]),
    ("append-grow", [["append", "k", "ab"], ["append", "k", "cd"], ["get", "k"], ["strlen", "k"]]),
    ("setrange-int-enc", [["set", "n", "12345"], ["setrange", "n", "0", "9"],
                          ["get", "n"], ["object", "encoding", "n"]]),
    ("setbit-int-enc", [["set", "n", "12345"], ["setbit", "n", "0", "1"],
                        ["object", "encoding", "n"]]),
    ("getrange-int", [["set", "n", "12345"], ["getrange", "n", "0", "2"]]),
]


def main():
    od = R(int(sys.argv[1]))
    fr = R(int(sys.argv[2]))
    div = 0
    for label, cmds in SEQS:
        od.cmd("flushall")
        fr.cmd("flushall")
        for c in cmds:
            ro = od.cmd(*c)
            rf = fr.cmd(*c)
            if ro != rf:
                div += 1
                print(f"DIVERGE {label} [{' '.join(c)}]\n  oracle: {ro}\n  fr    : {rf}")
    print("-" * 60)
    if div:
        print(f"FAIL — {div} divergence(s)")
        return 1
    print(f"PASS — string-growth surface byte-exact vs redis 7.2.4 ({len(SEQS)} sequences)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
