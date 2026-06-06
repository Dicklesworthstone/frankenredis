#!/usr/bin/env python3
"""debug_reload_check.py — RDB round-trip fidelity gate via DEBUG RELOAD.

Builds a comprehensive dataset across every type/encoding (string int/embstr/raw,
list listpack/quicklist, set intset/listpack/hashtable, hash listpack/hashtable,
zset listpack/skiplist, stream with consumer-group + consumer + PEL, HLL, TTLs),
then DEBUG RELOADs (serialize to RDB → reload) and asserts every key survives
identically. This exercises the entire RDB encode→decode path in-process — the
most intricate serialization code — and catches any regression that would corrupt
a value, drop a key, or change a type/TTL across persistence.

This is an fr-SELF consistency gate (not differential): the vendored oracle does
not enable DEBUG, so it cannot DEBUG RELOAD. The behavior asserted here is purely
internal RDB round-trip stability, which is well-defined regardless of the oracle.

INVARIANTS asserted before == after RELOAD:
  - TYPE, full VALUE/contents, and TTL band for every key, and DBSIZE.
  - OBJECT ENCODING preserved, EXCEPT a small string may legitimately reload from
    `raw` to `embstr`: redis (and fr) re-create strings via createStringObject on
    load, which picks embstr for <=44 bytes regardless of the pre-save encoding.
    That reclassification is correct, not a regression, so it is tolerated.

Requires the server started with --enable-debug-command yes|local.
Usage: debug_reload_check.py [--fr 16400]
Exit 0 if the round-trip preserves all data, else 1.
"""
import argparse
import sys


import socket


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(5.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def _rn(self, n):
        while len(self.b) < n + 2:
            self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t == b"+":
            return r.decode("latin1")
        if t == b":":
            return int(r)
        if t == b"-":
            return "ERR:" + r.decode("latin1")
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


def build(c):
    c.cmd("FLUSHALL")
    c.cmd("SET", "s_int", "12345")
    c.cmd("SET", "s_embstr", "hello world")
    c.cmd("SET", "s_raw", "x" * 100)
    c.cmd("SET", "s_ttl", "v", "EX", "10000")
    c.cmd("RPUSH", "l_lp", "a", "b", "c")
    c.cmd("RPUSH", "l_ql", *[f"item{i}" for i in range(300)])
    c.cmd("SADD", "set_int", "1", "2", "3", "99")
    c.cmd("SADD", "set_lp", "a", "b", "c")
    c.cmd("SADD", "set_ht", *[f"m{i}" for i in range(300)])
    c.cmd("HSET", "h_lp", "f1", "1", "f2", "2")
    c.cmd("HSET", "h_ht", *sum([[f"f{i}", str(i)] for i in range(300)], []))
    c.cmd("ZADD", "z_lp", "1", "a", "2.5", "b")
    c.cmd("ZADD", "z_sk", *sum([[str(i), f"m{i}"] for i in range(300)], []))
    c.cmd("XADD", "strm", "1-1", "field", "val")
    c.cmd("XADD", "strm", "2-2", "f2", "v2")
    c.cmd("XGROUP", "CREATE", "strm", "g1", "0")
    c.cmd("XREADGROUP", "GROUP", "g1", "consumer1", "COUNT", "1", "STREAMS", "strm", ">")
    c.cmd("PFADD", "hll", "a", "b", "c", "d", "e")


KEYS = [
    "s_int", "s_embstr", "s_raw", "s_ttl", "l_lp", "l_ql",
    "set_int", "set_lp", "set_ht", "h_lp", "h_ht", "z_lp", "z_sk", "strm", "hll",
]


def value_of(c, k, t):
    if t == "string":
        return c.cmd("GET", k)
    if t == "list":
        return c.cmd("LRANGE", k, "0", "-1")
    if t == "set":
        return sorted(c.cmd("SMEMBERS", k) or [])
    if t == "hash":
        flat = c.cmd("HGETALL", k) or []
        return sorted((flat[i], flat[i + 1]) for i in range(0, len(flat), 2))
    if t == "zset":
        return c.cmd("ZRANGE", k, "0", "-1", "WITHSCORES")
    if t == "stream":
        return (c.cmd("XLEN", k), c.cmd("XRANGE", k, "-", "+"),
                c.cmd("XINFO", "GROUPS", k), c.cmd("XPENDING", k, "g1"))
    return None


def snapshot(c):
    snap = {}
    for k in KEYS:
        t = c.cmd("TYPE", k)
        enc = c.cmd("OBJECT", "ENCODING", k)
        ttl = c.cmd("TTL", k)
        # bucket the TTL so a 1s tick between snapshots isn't a false diff
        ttl_band = "none" if ttl == -1 else ("missing" if ttl == -2 else "set")
        snap[k] = {"type": t, "enc": enc, "ttl": ttl_band, "val": value_of(c, k, t)}
    snap["__dbsize__"] = c.cmd("DBSIZE")
    return snap


def enc_ok(before, after):
    """Encoding preserved, tolerating the correct small-string raw->embstr
    reload reclassification."""
    if before == after:
        return True
    return {before, after} == {"raw", "embstr"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    c = Conn(args.fr)

    if c.cmd("DEBUG", "RELOAD").startswith("ERR"):
        print("FAIL: DEBUG not enabled — start the server with "
              "--enable-debug-command yes", file=sys.stderr)
        sys.exit(2)

    build(c)
    before = snapshot(c)
    reload_reply = c.cmd("DEBUG", "RELOAD")
    if reload_reply != "OK":
        print(f"FAIL: DEBUG RELOAD returned {reload_reply!r}")
        sys.exit(1)
    after = snapshot(c)

    diffs = 0
    for k in before:
        b, a = before[k], after.get(k)
        if k == "__dbsize__":
            if b != a:
                diffs += 1
                print(f"DIFF [DBSIZE] before={b} after={a}")
            continue
        if a is None:
            diffs += 1
            print(f"DIFF [{k}] key vanished after RELOAD")
            continue
        if b["type"] != a["type"] or b["ttl"] != a["ttl"] or b["val"] != a["val"]:
            diffs += 1
            print(f"DIFF [{k}] type/ttl/value changed across RELOAD:")
            print(f"   before: {b}")
            print(f"   after : {a}")
        elif not enc_ok(b["enc"], a["enc"]):
            diffs += 1
            print(f"DIFF [{k}] encoding {b['enc']} -> {a['enc']} (not a "
                  f"small-string embstr reclassification)")

    if diffs:
        print(f"\nFAIL: {diffs} DEBUG RELOAD round-trip divergences")
        sys.exit(1)
    print(f"OK: {len(KEYS)} keys across all type/encoding survive DEBUG RELOAD "
          "identically (RDB round-trip fidelity)")


if __name__ == "__main__":
    main()
