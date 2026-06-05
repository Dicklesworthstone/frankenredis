#!/usr/bin/env python3
"""sort_differ.py — differential fuzzer for the SORT / SORT_RO command.

Drives an exhaustive grid of SORT option combinations (BY hash/string/nosort
patterns, ASC/DESC, ALPHA, GET #/pattern, LIMIT, STORE) against fr-server and
the vendored redis 7.2.4 oracle, comparing the reply (and the stored list when
STORE is used) after each. Targets the order-sensitive corners: BY-nosort
direction per source type (list/zset/set), numeric/alpha tie-breaking under
DESC, and missing-key weight handling.

Both servers MUST run config-less (compiled defaults) so encodings align.

Usage: sort_differ.py [--oracle 16399] [--fr 16400] [--seed N] [--max 4000]
"""
import argparse
import random
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
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

    def _parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t in (b"+", b":"):
            return r.decode()
        if t == b"-":
            return "ERR:" + r.decode()
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode()
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self._parse() for _ in range(n)]
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self._parse()


def setup(c):
    c.cmd("FLUSHALL")
    c.cmd("RPUSH", "L", "3", "1", "2", "10", "5", "7", "3")  # dup 3 -> numeric tie
    c.cmd("SADD", "Sint", "3", "1", "2", "10", "5", "7")
    c.cmd("SADD", "Sstr", "banana", "apple", "cherry", "date")
    c.cmd("ZADD", "Z", "100", "3", "50", "1", "75", "2", "10", "10", "5", "5", "60", "7")
    for v in ("1", "2", "3", "5", "7", "10"):
        c.cmd("SET", f"w_{v}", str(100 - int(v)))
        c.cmd("SET", f"tie_{v}", "42")  # constant weight -> all tie
        c.cmd("HSET", f"h_{v}", "field", str(int(v) * 2))
        c.cmd("SET", f"d_{v}", f"D{v}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--max", type=int, default=4000)
    args = ap.parse_args()

    o, f = Conn(args.oracle), Conn(args.fr)
    for c in (o, f):
        if c.cmd("PING") != "PONG":
            print("server not responding", file=sys.stderr)
            sys.exit(2)

    keys = ["L", "Sint", "Sstr", "Z"]
    # `tie_*` (all-equal weight) and `miss_*` (all-missing weight) only produce a
    # DETERMINISTIC order under NUMERIC sorting, where upstream sortCompare breaks
    # ties by comparing the elements themselves. Under ALPHA + BY, tied by-keys
    # make sortCompare return 0 → unstable qsort → the order among equal-weight
    # elements is officially unspecified (redis SORT docs), so we exclude those
    # combos from the differential rather than chase glibc qsort's internal order.
    bys_numeric = [None, "nostar", "w_*", "tie_*", "h_*->field", "miss_*"]
    bys_alpha = [None, "nostar", "w_*", "h_*->field"]
    combos = []
    for k in keys:
        for alpha in [False, True]:
            for by in (bys_alpha if alpha else bys_numeric):
                for order in [None, "ASC", "DESC"]:
                    for get in [[], ["#"], ["#", "d_*"]]:
                        for store in [None, "dst"]:
                            for limit in [None, ("1", "3"), ("0", "-1"), ("2", "2")]:
                                cmd = ["SORT", k]
                                if by is not None:
                                    cmd += ["BY", by]
                                if order:
                                    cmd.append(order)
                                if alpha:
                                    cmd.append("ALPHA")
                                for g in get:
                                    cmd += ["GET", g]
                                if limit:
                                    cmd += ["LIMIT", limit[0], limit[1]]
                                if store:
                                    cmd += ["STORE", store]
                                combos.append(cmd)
    random.seed(args.seed)
    random.shuffle(combos)
    combos = combos[: args.max]

    ndiff = 0
    for cmd in combos:
        setup(o)
        setup(f)
        ro, rf = o.cmd(*cmd), f.cmd(*cmd)
        if "STORE" in cmd:
            ro = (ro, o.cmd("LRANGE", "dst", "0", "-1"))
            rf = (rf, f.cmd("LRANGE", "dst", "0", "-1"))
        if ro != rf:
            ndiff += 1
            if ndiff <= 12:
                print("DIFF", cmd)
                print("   oracle:", ro)
                print("   fr    :", rf)
    if ndiff:
        print(f"FAIL: {len(combos)} combos, {ndiff} divergences (seed {args.seed})")
        sys.exit(1)
    print(f"OK: {len(combos)} SORT combos, seed {args.seed} — byte-exact vs redis 7.2.4")


if __name__ == "__main__":
    main()
