#!/usr/bin/env python3
"""broad_command_headtohead.py — pipelined fr-vs-Redis-7.2.4 throughput sweep over
COMPUTE-HEAVY commands that the standard redis-benchmark set (13 cmds) does not
exercise, to surface clean per-command gaps the scorecard misses.

`scripts/bench_vs_redis.py` covers the canonical redis-benchmark tests
(get/set/incr/lpush/.../mset). This complements it: it preloads a fixed dataset
into BOTH servers, then pipelines a batch of each command and times it, reporting
fr/redis ratio (>1 = fr faster). Flags commands below 0.9x as losses.

This is how SINTERSTORE/SDIFFSTORE (0.55-0.64x) were found and fixed (a3310a98d:
direct SetValue build) — they were a cluster of set-algebra losses hidden by the
13-command scorecard. Known residual losses it still reports: sintercard (read
path), zcount (constant-factor), SINTER read.

Both servers must be running (start fr + vendored redis-server on free high ports).
Usage: broad_command_headtohead.py [fr_port] [redis_port] [--pipe N] [--trials T]
Exit 0 always (informational). Ratio = redis_ms/fr_ms (>1.05 fr faster, <0.9 loss).
"""
import socket
import sys
import time
import statistics


def opt(flag, default):
    return sys.argv[sys.argv.index(flag) + 1] if flag in sys.argv else default


FR = int(sys.argv[1]) if len(sys.argv) > 1 and not sys.argv[1].startswith("-") else 17811
RED = int(sys.argv[2]) if len(sys.argv) > 2 and not sys.argv[2].startswith("-") else 17812
PIPE = int(opt("--pipe", "200"))
TRIALS = int(opt("--trials", "7"))


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 5)
        self.s.settimeout(30)
        self.b = b""

    def _f(self):
        d = self.s.recv(1 << 16)
        if not d:
            raise EOFError
        self.b += d

    def _l(self):
        while b"\r\n" not in self.b:
            self._f()
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def read(self):
        l = self._l()
        t, r = l[:1], l[1:]
        if t in (b"+", b"-", b":"):
            return r
        if t == b"$":
            n = int(r)
            if n < 0:
                return None
            while len(self.b) < n + 2:
                self._f()
            d, self.b = self.b[:n], self.b[n + 2:]
            return d
        if t == b"*":
            n = int(r)
            if n < 0:
                return None
            return [self.read() for _ in range(n)]
        return l

    def cmd(self, *a):
        self.s.sendall(self._enc([a]))
        return self.read()

    def pipe(self, cmds):
        self.s.sendall(self._enc(cmds))
        return [self.read() for _ in cmds]

    @staticmethod
    def _enc(cmds):
        buf = []
        for a in cmds:
            buf.append(b"*%d\r\n" % len(a))
            for x in a:
                x = x if isinstance(x, (bytes, bytearray)) else str(x).encode()
                buf.append(b"$%d\r\n%s\r\n" % (len(x), x))
        return b"".join(buf)


def setup(c):
    c.cmd("FLUSHALL")
    c.cmd("SET", "bigstr", "x" * 20000)
    c.cmd("SADD", "setA", *[f"m{j}" for j in range(2000)])
    c.cmd("SADD", "setB", *[f"m{j}" for j in range(1000, 3000)])
    c.cmd("SADD", "setC", *[f"m{j}" for j in range(500, 1500)])
    c.cmd("ZADD", "bigz", *[x for j in range(2000) for x in (j, f"zm{j}")])
    c.cmd("HSET", "bigh", *[x for j in range(1000) for x in (f"f{j}", f"v{j}")])
    c.cmd("RPUSH", "biglist", *[f"e{j}" for j in range(2000)])


WORK = {
    "getrange": ["GETRANGE", "bigstr", 0, 10000],
    "bitcount": ["BITCOUNT", "bigstr"],
    "sintercard": ["SINTERCARD", 2, "setA", "setB"],
    "sinterstore": ["SINTERSTORE", "dst", "setA", "setB"],
    "sunionstore": ["SUNIONSTORE", "dst", "setA", "setB"],
    "sdiffstore": ["SDIFFSTORE", "dst", "setA", "setB"],
    "sinter3": ["SINTER", "setA", "setB", "setC"],
    "smismember": ["SMISMEMBER", "setA"] + [f"m{j}" for j in range(0, 200, 2)],
    "zrangebyscore": ["ZRANGEBYSCORE", "bigz", 500, 1500],
    "zrange_rev": ["ZRANGE", "bigz", 0, 200, "REV"],
    "hrandfield": ["HRANDFIELD", "bigh", 100],
    "zrandmember": ["ZRANDMEMBER", "bigz", 100],
    "srandmember": ["SRANDMEMBER", "setA", 100],
    "lrange_full": ["LRANGE", "biglist", 0, -1],
    "lpos": ["LPOS", "biglist", "e1999"],
    "zcount": ["ZCOUNT", "bigz", 500, 1500],
}


def main():
    fr, red = Conn(FR), Conn(RED)
    setup(fr)
    setup(red)
    print(f"fr:{FR} redis:{RED}  pipe={PIPE} trials={TRIALS}")
    print(f"{'cmd':<16}{'fr_ms':>8}{'redis_ms':>9}{'ratio':>7}  verdict")
    losses = []
    for name, c in WORK.items():
        batch = [c] * PIPE

        def b(conn):
            t = time.perf_counter()
            conn.pipe(batch)
            return time.perf_counter() - t
        b(fr)
        b(red)
        rf = sorted(b(fr) for _ in range(TRIALS))
        rr = sorted(b(red) for _ in range(TRIALS))
        mf, mr = statistics.median(rf), statistics.median(rr)
        ratio = mr / mf
        v = "fr" if ratio > 1.05 else ("REDIS" if ratio < 0.9 else "~")
        if ratio < 0.9:
            losses.append((name, round(ratio, 3)))
        print(f"{name:<16}{mf*1000:>8.1f}{mr*1000:>9.1f}{ratio:>7.2f}  {v}")
    print("LOSSES(<0.9x):", sorted(losses, key=lambda x: x[1]))


if __name__ == "__main__":
    main()
