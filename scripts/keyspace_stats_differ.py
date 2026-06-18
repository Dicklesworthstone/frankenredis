#!/usr/bin/env python3
"""keyspace_stats_differ.py — INFO keyspace_hits/misses differential vs redis 7.2.4.

A novel parity dimension the reply/state/encoding differs don't cover: how many
keyspace lookups (hits/misses) a command records. Redis counts one lookupKeyRead
per accessed key; this gate covers set/zset algebra commands that historically
over-counted when probing one key's dict once per element of another.

For each case: FLUSHALL + identical seed on both, CONFIG RESETSTAT, run the command,
then compare keyspace_hits and keyspace_misses. Reports per-command divergence.

Usage: keyspace_stats_differ.py <oracle_port> <fr_port>
"""
import socket, sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(10); self.b = b""
    def _l(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d: raise OSError("closed")
            self.b += d
        l, self.b = self.b.split(b"\r\n", 1); return l
    def _n(self, n):
        while len(self.b) < n + 2: self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n+2:]; return d
    def parse(self):
        l = self._l(); t, r = l[:1], l[1:]
        if t == b"$":
            n = int(r); return None if n < 0 else self._n(n)
        if t == b":": return int(r)
        if t in (b"+", b"-"): return r
        if t == b"*":
            n = int(r); return None if n < 0 else [self.parse() for _ in range(n)]
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o); return self.parse()


def stats(c):
    info = c.cmd("INFO", "stats").decode(errors="replace")
    h = m = 0
    for line in info.splitlines():
        if line.startswith("keyspace_hits:"): h = int(line.split(":")[1])
        elif line.startswith("keyspace_misses:"): m = int(line.split(":")[1])
    return h, m


SEED = [
    ("ZADD", "za", "1", "a", "2", "b", "3", "c", "4", "d", "5", "e"),
    ("ZADD", "zb", "1", "a", "2", "b", "3", "c"),
    ("ZADD", "zc", "1", "c", "2", "d", "3", "e", "4", "f"),
    ("SADD", "sa", "a", "b", "c", "d", "e"),
    ("SADD", "sb", "a", "b", "c"),
    ("SET", "str", "hello"),
    ("RPUSH", "lst", "x", "y", "z"),
    ("HSET", "hsh", "f1", "v1", "f2", "v2"),
]

CASES = [
    ("ZINTERCARD 2 za zb", ["ZINTERCARD", "2", "za", "zb"]),
    ("ZINTER 2 za zb", ["ZINTER", "2", "za", "zb"]),
    ("ZINTER 3 za zb zc", ["ZINTER", "3", "za", "zb", "zc"]),
    ("ZDIFF 2 za zb", ["ZDIFF", "2", "za", "zb"]),
    ("ZDIFFSTORE d 2 za zb", ["ZDIFFSTORE", "d", "2", "za", "zb"]),
    ("ZUNION 2 za zb", ["ZUNION", "2", "za", "zb"]),
    ("ZUNIONSTORE d 2 za zb", ["ZUNIONSTORE", "d", "2", "za", "zb"]),
    ("ZINTERSTORE d 2 za zb", ["ZINTERSTORE", "d", "2", "za", "zb"]),
    ("SINTERCARD 2 sa sb", ["SINTERCARD", "2", "sa", "sb"]),
    ("SINTER sa sb", ["SINTER", "sa", "sb"]),
    ("SDIFF sa sb", ["SDIFF", "sa", "sb"]),
    ("SUNION sa sb", ["SUNION", "sa", "sb"]),
    ("SINTERSTORE d sa sb", ["SINTERSTORE", "d", "sa", "sb"]),
    ("SDIFFSTORE d sa sb", ["SDIFFSTORE", "d", "sa", "sb"]),
    ("SMISMEMBER sa a b x", ["SMISMEMBER", "sa", "a", "b", "x"]),
    ("ZMSCORE za a b x", ["ZMSCORE", "za", "a", "b", "x"]),
    ("MGET str za sa", ["MGET", "str", "za", "sa"]),
    ("EXISTS za zb sa str", ["EXISTS", "za", "zb", "sa", "str"]),
    ("GETRANGE str 0 -1", ["GETRANGE", "str", "0", "-1"]),
    ("LRANGE lst 0 -1", ["LRANGE", "lst", "0", "-1"]),
    ("HGETALL hsh", ["HGETALL", "hsh"]),
]


def run(oport, fport):
    o, f = Conn(oport), Conn(fport)
    diverged = []
    for label, cmd in CASES:
        for c in (o, f):
            c.cmd("FLUSHALL")
            for s in SEED:
                c.cmd(*s)
            c.cmd("CONFIG", "RESETSTAT")
            c.cmd(*cmd)
        oh, om = stats(o)
        fh, fm = stats(f)
        if (oh, om) != (fh, fm):
            diverged.append((label, (oh, om), (fh, fm)))
    return diverged


def main():
    oport, fport = int(sys.argv[1]), int(sys.argv[2])
    d = run(oport, fport)
    for label, ored, fred in d:
        print(f"  {label:28s} redis={ored} fr={fred}")
    if d:
        print(f"FAIL — {len(d)} keyspace-stats divergence(s) vs redis 7.2.4")
        return 1
    print(f"PASS — keyspace_hits/misses match redis 7.2.4 across {len(CASES)} "
          "commands")
    return 0


if __name__ == "__main__":
    sys.exit(main())
