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

    # (frankenredis-p98mw) GLOBAL WARM-UP before any timing, so the first WORK entry is not
    # measured on cold sockets, a cold allocator and a ramping clock.
    #
    # HONESTY NOTE: this did NOT measurably tighten the A/A null. Old vs new, same pair of
    # identical binaries, same window: spread 0.84-1.01 before, 0.83-1.01 after. It is kept
    # on methodological grounds only. The null of this harness FAILS (band ~0.83-1.27) and
    # the command it fails on MIGRATES between windows -- getrange nulled 0.67 in one and
    # 0.91 in another; sintercard nulled 1.22 in one and 0.83 in another with no change to
    # its position. The mechanism is UNIDENTIFIED. Do not read any ratio inside that band.
    for _ in range(2):
        for _n, _c in WORK.items():
            fr.pipe([_c] * PIPE)
            red.pipe([_c] * PIPE)

    for name, c in WORK.items():
        batch = [c] * PIPE

        def b(conn):
            t = time.perf_counter()
            conn.pipe(batch)
            return time.perf_counter() - t
        b(fr)
        b(red)
        # (frankenredis-p98mw) INTERLEAVED AND ALTERNATING (ABBA), not two blocks. Measuring
        # all TRIALS of fr and then all TRIALS of redis makes any monotonic drift across the
        # pair -- frequency ramp, a neighbour's build starting, page-cache warming -- land on
        # one arm as signal. Interleaving makes it common-mode; alternating which arm goes
        # first cancels the residual within-pair ordering bias. The instruction harness in
        # this repo already measures ABBA per shape; this one did not.
        #
        # This removes a real bias CLASS but is NOT the fix for this harness's null -- see
        # the honesty note above. It is strictly better method, not a measured improvement.
        rf, rr = [], []
        for i in range(TRIALS):
            if i % 2 == 0:
                rf.append(b(fr))
                rr.append(b(red))
            else:
                rr.append(b(red))
                rf.append(b(fr))
        rf.sort()
        rr.sort()
        mf, mr = statistics.median(rf), statistics.median(rr)
        ratio = mr / mf
        v = "fr" if ratio > 1.05 else ("REDIS" if ratio < 0.9 else "~")
        if ratio < 0.9:
            losses.append((name, round(ratio, 3)))
        print(f"{name:<16}{mf*1000:>8.1f}{mr*1000:>9.1f}{ratio:>7.2f}  {v}")
    print("LOSSES(<0.9x):", sorted(losses, key=lambda x: x[1]))


if __name__ == "__main__":
    main()
