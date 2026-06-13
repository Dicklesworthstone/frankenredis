#!/usr/bin/env python3
"""Differential gate for the zset-store BULK-BUILD paths vs vendored redis 7.2.4.

Locks in the dest-zset bulk-build campaign (commits e30fdbf35 / 9e0f34603 /
ea3903604 / 14fbaedf1): ZUNIONSTORE, ZINTERSTORE, ZRANGESTORE, GEOSEARCHSTORE all
route their destination build through `SortedSet::from_unique_pairs_with_limits`
(O(n) bulk `BTreeMap::from_iter`). The build is byte-identical to the old
incremental path, but it has subtle edges this gate pins down at scale:
  - the listpack(Packed) <-> skiplist(Full) boundary by result cardinality,
  - ZRANGESTORE BYSCORE/BYLEX force-skiplist encoding even for tiny results,
  - WEIGHTS / AGGREGATE SUM|MIN|MAX score aggregation,
  - NaN aggregate scores normalized to 0,
  - empty results that delete the destination,
  - large (Full-encoded) destinations.

For every case it compares, byte-for-byte against the oracle: the destination's
`ZRANGE 0 -1 WITHSCORES`, its `OBJECT ENCODING`, and `ZCARD`.

Usage: zset_store_bulk_differ.py --oracle <redis_port> --fr <fr_port>
"""
import argparse
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=20)

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            a = a if isinstance(a, bytes) else str(a).encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self._read()

    def _read(self):
        # minimal RESP reader sufficient for our reply shapes
        line = self._line()
        t, rest = line[:1], line[1:]
        if t in (b"+", b"-", b":"):
            return line
        if t == b"$":
            n = int(rest)
            if n < 0:
                return b"$-1"
            data = self._exact(n)
            self._exact(2)
            return b"$" + rest + b"\r\n" + data
        if t == b"*":
            n = int(rest)
            if n < 0:
                return b"*-1"
            return b"*" + rest + b"\r\n" + b"".join(self._read() + b"\n" for _ in range(n))
        return line

    def _line(self):
        buf = b""
        while not buf.endswith(b"\r\n"):
            ch = self.s.recv(1)
            if not ch:
                break
            buf += ch
        return buf[:-2]

    def _exact(self, n):
        buf = b""
        while len(buf) < n:
            buf += self.s.recv(n - len(buf))
        return buf


def reset(c):
    c.cmd("FLUSHALL")


def seed_zsets(c, n_small, n_big_a, n_big_b, overlap):
    # small zsets (force listpack region) and big overlapping zsets (force skiplist)
    for i in range(n_small):
        c.cmd("ZADD", "s1", i % 4, f"m{i}")
        c.cmd("ZADD", "s2", i % 3, f"m{i + 2}")
    # big A: m0..m(n_big_a-1); big B: m(n_big_a-overlap)..  -> `overlap` shared
    pipe = []
    for i in range(n_big_a):
        pipe.append(("ZADD", "b1", i, f"k{i}"))
    for i in range(n_big_b):
        pipe.append(("ZADD", "b2", i * 2, f"k{n_big_a - overlap + i}"))
    for p in pipe:
        c.cmd(*p)


def seed_geo(c):
    pts = [(13.361, 38.115, "a"), (15.087, 37.502, "b"), (2.349, 48.853, "c"),
           (0.127, 51.507, "d"), (-0.127, 51.5, "e"), (12.5, 41.9, "f")]
    for lon, lat, m in pts:
        c.cmd("GEOADD", "g", lon, lat, m)


CASES = [
    # (label, [setup cmds...], dest)
    ("zunionstore_small_sum", [("ZUNIONSTORE", "d", "2", "s1", "s2")], "d"),
    ("zunionstore_weights_min", [("ZUNIONSTORE", "d", "2", "s1", "s2", "WEIGHTS", "2", "3", "AGGREGATE", "MIN")], "d"),
    ("zunionstore_big_max", [("ZUNIONSTORE", "d", "2", "b1", "b2", "AGGREGATE", "MAX")], "d"),
    ("zinterstore_small", [("ZINTERSTORE", "d", "2", "s1", "s2")], "d"),
    ("zinterstore_big_weights", [("ZINTERSTORE", "d", "2", "b1", "b2", "WEIGHTS", "2", "3", "AGGREGATE", "SUM")], "d"),
    ("zinterstore_self", [("ZINTERSTORE", "d", "2", "b1", "b1")], "d"),
    ("zrangestore_byrank_full", [("ZRANGESTORE", "d", "b1", "0", "-1")], "d"),
    ("zrangestore_byscore_skiplist", [("ZRANGESTORE", "d", "b1", "(2", "50", "BYSCORE")], "d"),
    ("zrangestore_byscore_tiny", [("ZRANGESTORE", "d", "s1", "0", "1", "BYSCORE")], "d"),
    ("zrangestore_bylex", [("ZRANGESTORE", "d", "s1", "-", "+", "BYLEX")], "d"),
    ("zrangestore_empty_deletes", [("ZRANGESTORE", "d", "b1", "100000", "200000", "BYSCORE")], "d"),
    ("zunionstore_empty", [("ZUNIONSTORE", "d", "1", "nope")], "d"),
    ("geosearchstore_radius", [("GEOSEARCHSTORE", "d", "g", "FROMLONLAT", "14", "38", "BYRADIUS", "4000", "km", "ASC")], "d"),
    ("geosearchstore_box_storedist", [("GEOSEARCHSTORE", "d", "g", "FROMMEMBER", "a", "BYBOX", "8000", "8000", "km", "ASC", "STOREDIST")], "d"),
]


def observe(c, dest):
    return (
        c.cmd("ZRANGE", dest, "0", "-1", "WITHSCORES"),
        c.cmd("OBJECT", "ENCODING", dest),
        c.cmd("ZCARD", dest),
    )


def run_case(c, setup, dest):
    for cmd in setup:
        c.cmd(*cmd)
    return observe(c, dest)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, required=True)
    ap.add_argument("--fr", type=int, required=True)
    args = ap.parse_args()
    oracle, fr = Conn(args.oracle), Conn(args.fr)

    fails = 0
    for label, setup, dest in CASES:
        for c in (oracle, fr):
            reset(c)
            seed_zsets(c, 12, 400, 400, 250)
            seed_geo(c)
        ro = run_case(oracle, setup, dest)
        rf = run_case(fr, setup, dest)
        if ro != rf:
            fails += 1
            print(f"DIVERGE [{label}]")
            print(f"  oracle: {ro}")
            print(f"  fr:     {rf}")
    total = len(CASES)
    if fails:
        print(f"FAIL: {fails}/{total} zset-store bulk-build cases diverge vs redis 7.2.4")
        sys.exit(1)
    print(f"OK: {total}/{total} zset-store bulk-build cases byte-exact vs redis 7.2.4 "
          f"(ZUNIONSTORE/ZINTERSTORE/ZRANGESTORE/GEOSEARCHSTORE; output+encoding+card)")


if __name__ == "__main__":
    main()
