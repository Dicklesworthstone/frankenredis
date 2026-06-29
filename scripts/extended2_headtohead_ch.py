#!/usr/bin/env python3
"""extended2_headtohead_ch.py (CrimsonHawk) — round-2 pipelined fr-vs-Redis-7.2.4 sweep
over compute-heavy commands NOT covered by broad_command_headtohead.py or
extended_headtohead_ch.py. Ratio = redis_ms/fr_ms (>1.05 fr faster, <0.9 loss)."""
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
            out += [b"$%d\r\n" % len(p), p, b"\r\n"]
        return b"".join(out)

    def _one(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
        nl = self.b.find(b"\r\n"); line = self.b[:nl]; self.b = self.b[nl + 2:]
        t = line[:1]
        if t in (b"+", b"-", b":"):
            return line
        if t == b"$":
            n = int(line[1:])
            if n < 0:
                return None
            while len(self.b) < n + 2:
                self.b += self.s.recv(65536)
            d = self.b[:n]; self.b = self.b[n + 2:]; return d
        if t == b"*":
            return [self._one() for _ in range(int(line[1:]))]
        return line

    def cmd(self, *parts):
        self.s.sendall(self._enc(parts)); return self._one()

    def pipe(self, batch):
        self.s.sendall(b"".join(self._enc(c) for c in batch))
        for _ in batch:
            self._one()


def setup(c):
    c.cmd("FLUSHALL")
    c.cmd("SET", "bigstr", "x" * 20000)
    c.cmd("SET", "numstr", "123456789")
    c.cmd("SADD", "setA", *[f"m{j}" for j in range(2000)])
    c.cmd("SADD", "intset", *[str(j) for j in range(500)])
    c.cmd("ZADD", "bigz", *[x for j in range(2000) for x in (j, f"zm{j}")])
    c.cmd("ZADD", "ov1", *[x for j in range(2000) for x in (j, f"k{j}")])
    c.cmd("ZADD", "ov2", *[x for j in range(2000) for x in (j * 2, f"k{j}")])
    c.cmd("HSET", "bigh", *[x for j in range(1000) for x in (f"f{j}", f"v{j}")])
    c.cmd("RPUSH", "biglist", *[f"e{j}" for j in range(2000)])
    c.cmd("RPUSH", "dlist", *[f"d{j % 50}" for j in range(2000)])  # many dups for LREM/LPOS


WORK = {
    "zrangestore": ["ZRANGESTORE", "zdst", "bigz", 0, 500],
    "zmpop": ["ZMPOP", 1, "bigz", "MIN", "COUNT", 1],
    "zpopmin_n": ["ZPOPMIN", "ov1", 1],
    "zincrby": ["ZINCRBY", "bigz", 1, "zm1000"],
    "zscore": ["ZSCORE", "bigz", "zm1000"],
    "zrank": ["ZRANK", "bigz", "zm1000"],
    "zrank_ws": ["ZRANK", "bigz", "zm1000", "WITHSCORE"],
    "smove": ["SMOVE", "setA", "setB", "m1999"],
    "spop_n": ["SPOP", "intset", 1],
    "srandmember_neg": ["SRANDMEMBER", "setA", -50],
    "sintercard_lim": ["SINTERCARD", 2, "ov1", "setA", "LIMIT", 10],
    "lrem": ["LREM", "dlist", 0, "d7"],
    "lpos_count": ["LPOS", "dlist", "d7", "COUNT", 0],
    "linsert": ["LINSERT", "biglist", "BEFORE", "e1000", "newval"],
    "lmpop": ["LMPOP", 1, "biglist", "LEFT", "COUNT", 1],
    "getdel": ["GETDEL", "missingk"],
    "getex": ["GETEX", "numstr"],
    "append": ["APPEND", "numstr", "z"],
    "setbit": ["SETBIT", "bigstr", 100000, 1],
    "getbit": ["GETBIT", "bigstr", 5000],
    "bitop_and": ["BITOP", "AND", "bdst", "bigstr", "bigstr"],
    "incr": ["INCR", "ctr"],
    "incrbyfloat": ["INCRBYFLOAT", "fctr", 1.5],
    "hincrbyfloat": ["HINCRBYFLOAT", "bigh", "f1", 1.5],
    "hrandfield_neg": ["HRANDFIELD", "bigh", -50],
    "copy_list": ["COPY", "biglist", "biglist_cp", "REPLACE"],
    "object_freq": ["OBJECT", "REFCOUNT", "bigh"],
    "type": ["TYPE", "bigz"],
    "strlen": ["STRLEN", "bigstr"],
    "zremrangebyrank": ["ZREMRANGEBYRANK", "nope", 0, 10],
}


def main():
    fr, red = Conn(FR), Conn(RED)
    setup(fr); setup(red)
    print(f"fr:{FR} redis:{RED}  pipe={PIPE} trials={TRIALS}")
    print(f"{'cmd':<18}{'fr_ms':>8}{'redis_ms':>9}{'ratio':>7}  verdict")
    losses = []
    for name, c in WORK.items():
        batch = [c] * PIPE

        def b(conn):
            t = time.perf_counter(); conn.pipe(batch); return time.perf_counter() - t
        b(fr); b(red)
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
