#!/usr/bin/env python3
"""extended_headtohead_ch.py (CrimsonHawk) — pipelined fr-vs-Redis-7.2.4 sweep over
compute-heavy commands NOT in broad_command_headtohead.py, to surface fresh long-tail
gaps. Ratio = redis_ms/fr_ms (>1.05 fr faster, <0.9 loss). Exit 0 (informational)."""
import socket, sys, time, statistics


def opt(flag, default):
    return sys.argv[sys.argv.index(flag) + 1] if flag in sys.argv else default


FR = int(sys.argv[1]) if len(sys.argv) > 1 and not sys.argv[1].startswith("-") else 17811
RED = int(sys.argv[2]) if len(sys.argv) > 2 and not sys.argv[2].startswith("-") else 17812
PIPE = int(opt("--pipe", "200"))
TRIALS = int(opt("--trials", "9"))


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 5)
        self.s.settimeout(30)
        self.b = b""

    def _enc(self, parts):
        out = [b"*%d\r\n" % len(parts)]
        for p in parts:
            p = str(p).encode() if not isinstance(p, bytes) else p
            out.append(b"$%d\r\n" % len(p))
            out.append(p)
            out.append(b"\r\n")
        return b"".join(out)

    def cmd(self, *parts):
        self.s.sendall(self._enc(parts))
        return self._read_one()

    def _read_one(self):
        while True:
            nl = self.b.find(b"\r\n")
            if nl != -1:
                break
            self.b += self.s.recv(65536)
        line = self.b[:nl]
        self.b = self.b[nl + 2:]
        t = line[:1]
        if t in (b"+", b"-", b":"):
            return line
        if t == b"$":
            n = int(line[1:])
            if n < 0:
                return None
            while len(self.b) < n + 2:
                self.b += self.s.recv(65536)
            d = self.b[:n]
            self.b = self.b[n + 2:]
            return d
        if t == b"*":
            n = int(line[1:])
            return [self._read_one() for _ in range(n)]
        return line

    def pipe(self, batch):
        buf = b"".join(self._enc(c) for c in batch)
        self.s.sendall(buf)
        for _ in batch:
            self._read_one()


def setup(c):
    c.cmd("FLUSHALL")
    c.cmd("SET", "bigstr", "x" * 20000)
    c.cmd("SADD", "setA", *[f"m{j}" for j in range(2000)])
    c.cmd("SADD", "setB", *[f"m{j}" for j in range(1000, 3000)])
    c.cmd("ZADD", "bigz", *[x for j in range(2000) for x in (j, f"zm{j}")])
    # lex-uniform zset (all score 0) for ZRANGEBYLEX/ZLEXCOUNT
    c.cmd("ZADD", "lexz", *[x for j in range(2000) for x in (0, f"{j:05d}")])
    c.cmd("ZADD", "bigz2", *[x for j in range(2000) for x in (j + 0.5, f"zn{j}")])
    c.cmd("HSET", "bigh", *[x for j in range(1000) for x in (f"f{j}", f"v{j}")])
    c.cmd("RPUSH", "biglist", *[f"e{j}" for j in range(2000)])
    c.cmd("PFADD", "hll", *[f"e{j}" for j in range(2000)])
    c.cmd("PFADD", "hll2", *[f"q{j}" for j in range(2000)])


WORK = {
    "zrangebylex": ["ZRANGEBYLEX", "lexz", "[00500", "[01500"],
    "zlexcount": ["ZLEXCOUNT", "lexz", "[00500", "[01500"],
    "zrevrangebylex": ["ZREVRANGEBYLEX", "lexz", "[01500", "[00500"],
    "zdiff": ["ZDIFF", 2, "bigz", "bigz2"],
    "zinter": ["ZINTER", 2, "bigz", "bigz2"],
    "zunion": ["ZUNION", 2, "bigz", "bigz2"],
    "zdiffstore": ["ZDIFFSTORE", "zdst", 2, "bigz", "bigz2"],
    "zrangebyscore_lim": ["ZRANGEBYSCORE", "bigz", 0, 2000, "LIMIT", 100, 200],
    "zrevrange": ["ZREVRANGE", "bigz", 0, 300, "WITHSCORES"],
    "hrandfield_wv": ["HRANDFIELD", "bigh", 100, "WITHVALUES"],
    "zrandmember_ws": ["ZRANDMEMBER", "bigz", 100, "WITHSCORES"],
    "getrange_mid": ["GETRANGE", "bigstr", 5000, 15000],
    "setrange": ["SETRANGE", "bigstr", 10000, "yyyy"],
    "bitpos": ["BITPOS", "bigstr", 1],
    "bitcount_range": ["BITCOUNT", "bigstr", 100, 10000, "BIT"],
    "pfcount": ["PFCOUNT", "hll"],
    "pfcount2": ["PFCOUNT", "hll", "hll2"],
    "lrange_mid": ["LRANGE", "biglist", 500, 1500],
    "lpos_rank": ["LPOS", "biglist", "e500", "RANK", 1, "COUNT", 0],
    "smembers": ["SMEMBERS", "setA"],
    "hvals": ["HVALS", "bigh"],
    "hkeys": ["HKEYS", "bigh"],
    "hgetall": ["HGETALL", "bigh"],
    "sort_list": ["SORT", "biglist", "ALPHA", "LIMIT", 0, 100],
    "object_enc": ["OBJECT", "ENCODING", "bigh"],
}


def main():
    fr, red = Conn(FR), Conn(RED)
    setup(fr)
    setup(red)
    print(f"fr:{FR} redis:{RED}  pipe={PIPE} trials={TRIALS}")
    print(f"{'cmd':<18}{'fr_ms':>8}{'redis_ms':>9}{'ratio':>7}  verdict")
    losses = []
    for name, c in WORK.items():
        batch = [c] * PIPE

        def b(conn):
            t = time.perf_counter()
            conn.pipe(batch)
            return time.perf_counter() - t
        b(fr); b(red)
        # interleave trials to cancel drift
        rf, rr = [], []
        for _ in range(TRIALS):
            rf.append(b(fr)); rr.append(b(red))
        rf.sort(); rr.sort()
        mf, mr = statistics.median(rf), statistics.median(rr)
        ratio = mr / mf
        v = "fr" if ratio > 1.05 else ("REDIS" if ratio < 0.9 else "~")
        if ratio < 0.9:
            losses.append((name, round(ratio, 3)))
        print(f"{name:<18}{mf*1000:>8.2f}{mr*1000:>9.2f}{ratio:>7.2f}  {v}")
    print("LOSSES(<0.9x):", sorted(losses, key=lambda x: x[1]))


if __name__ == "__main__":
    main()
