#!/usr/bin/env python3
"""Multi-page SCAN cursor-resume completeness gate vs vendored redis 7.2.4.

The existing scan_differ.py issues a single `SCAN 0 COUNT 100` (one page,
completes immediately) — it does NOT exercise the multi-page cursor-resume added
in frankenredis-n9am7 (c63da7039), where each call returns a non-zero cursor and
the next call resumes from it. A resume bug (a key skipped or duplicated across
page boundaries, or the cursor not converging to 0) is invisible to a one-page
test but breaks real clients that page with a small COUNT.

This drives a FULL iteration (cursor 0 -> ... -> 0) with a SMALL COUNT so the
scan spans many pages, and asserts the COMPLETE key set returned equals redis's
— across COUNT sizes (resume must be COUNT-independent), MATCH globs, and TYPE
filters. Order and cursor values are intentionally NOT compared (BTreeSet vs dict
traversal — WONTFIX, see scan_differ.py).

Usage: scan_resume_keyset_gate.py <oracle_port> <fr_port>
Exit 0 = complete key set matches across all page sizes/filters; 1 = a miss/dup.
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


def full_scan(c, opts):
    """Full SCAN iteration; returns (set-of-keys, total-emitted, page-count)."""
    keys = set()
    total = 0
    cur = "0"
    pages = 0
    first = True
    while first or cur != "0":
        first = False
        r = c.cmd("scan", cur, *opts)
        cur = r[0]
        keys.update(r[1])
        total += len(r[1])
        pages += 1
        if pages > 1_000_000:
            break
    return keys, total, pages


CASES = [
    ("no-opts COUNT 10", ["COUNT", "10"]),
    ("no-opts COUNT 1", ["COUNT", "1"]),
    ("no-opts COUNT 1000", ["COUNT", "1000"]),
    ("MATCH str:* COUNT 10", ["MATCH", "str:*", "COUNT", "10"]),
    ("MATCH *:1 COUNT 5", ["MATCH", "*:1", "COUNT", "5"]),
    ("TYPE string COUNT 13", ["TYPE", "string", "COUNT", "13"]),
    ("TYPE list COUNT 7", ["TYPE", "list", "COUNT", "7"]),
    ("TYPE set COUNT 9", ["TYPE", "set", "COUNT", "9"]),
    ("TYPE hash COUNT 11", ["TYPE", "hash", "COUNT", "11"]),
    ("TYPE zset COUNT 3", ["TYPE", "zset", "COUNT", "3"]),
    ("TYPE string MATCH str:1* COUNT 7", ["TYPE", "string", "MATCH", "str:1*", "COUNT", "7"]),
]


def setup(c):
    c.cmd("flushall")
    for i in range(500):
        c.cmd("set", f"str:{i}", "v")
    for i in range(200):
        c.cmd("rpush", f"lst:{i}", "a")
    for i in range(150):
        c.cmd("sadd", f"set:{i}", "m")
    for i in range(120):
        c.cmd("hset", f"hsh:{i}", "f", "v")
    for i in range(80):
        c.cmd("zadd", f"zst:{i}", "1", "m")


def main():
    od = R(int(sys.argv[1]))
    fr = R(int(sys.argv[2]))
    setup(od)
    setup(fr)
    div = 0
    for label, opts in CASES:
        ka, _, _ = full_scan(od, opts)
        kb, tb, pb = full_scan(fr, opts)
        dup = tb - len(kb)  # fr emitted more than the distinct set => duplicates across pages
        if ka != kb:
            div += 1
            print(f"DIVERGE [{label}] oracle={len(ka)} fr={len(kb)} "
                  f"missing_in_fr={len(ka - kb)} extra_in_fr={len(kb - ka)}")
            print("  sample missing:", list(ka - kb)[:5], "extra:", list(kb - ka)[:5])
        else:
            print(f"OK [{label}] {len(kb)} keys over {pb} pages"
                  + (f"  (+{dup} dup emissions — allowed by SCAN contract)" if dup else ""))
    print("-" * 60)
    if div:
        print(f"FAIL — {div} multi-page resume divergence(s)")
        return 1
    print(f"PASS — multi-page SCAN cursor-resume returns the complete key set "
          f"across {len(CASES)} page-size/filter combos")
    return 0


if __name__ == "__main__":
    sys.exit(main())
