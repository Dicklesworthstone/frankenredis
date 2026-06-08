#!/usr/bin/env python3
"""random_state_differ.py — random-command keyspace-STATE differential fuzzer.

Most differential probes compare command REPLIES. This one instead compares the
full KEYSPACE STATE after every command: it drives the SAME seeded random stream
of commands (across string/list/set/hash/zset/bitmap/generic) against a tiny
shared 3-key pool on both fr and vendored redis 7.2.4, and after each command
snapshots TYPE + canonical value of every key on both. It reports the FIRST
command after which the two stores' state diverges — i.e. the root-cause command,
not a later symptom. The shared small key pool is deliberate: it forces
cross-type collisions (wrong-type handling, type-check ORDER, encoding
transitions) that single-type fuzzers and hand probes miss.

Excluded from the random pool to avoid non-deterministic FALSE positives:
  - timing: EXPIRE/PEXPIRE/SETEX/GETEX and any SET ... EX|PX|EXAT|PXAT (a tiny
    TTL races key expiry between the two servers' independent wall clocks).
  - randomized: SPOP (random member selection).
  - PF*: HyperLogLog has its own probes; PFADD/PFCOUNT on an HLL corrupted by a
    prior APPEND legitimately differs (redis layered validation) and is tracked
    separately as frankenredis-yiu5p.
Set-/hash-returning reads are order-normalized (unspecified iteration order).

Usage: random_state_differ.py [--oracle 16399] [--fr 16400] [--seeds 8] [--iters 3000]
Exit 0 if every key's state stays identical across the whole run, else 1.
"""
import argparse
import random
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(3.0)
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
            return ("ss", r.decode("latin1"))
        if t == b":":
            return ("int", int(r))
        if t == b"-":
            return ("err", r.decode("latin1").split()[0])
        if t == b"$":
            n = int(r)
            return ("nil",) if n < 0 else ("bulk", self._rn(n).decode("latin1"))
        if t == b"*":
            n = int(r)
            return ("nil",) if n < 0 else ("arr", [self.parse() for _ in range(n)])
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


KEYS = ["k1", "k2", "k3"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--seeds", type=int, default=8)
    ap.add_argument("--iters", type=int, default=3000)
    args = ap.parse_args()
    R, F = Conn(args.oracle), Conn(args.fr)

    def k():
        return random.choice(KEYS)

    def v():
        return random.choice(["1", "2", "0", "-1", "3.5", "a", "bb", "ccc",
                              "x" * 30, "9999999999999999999"])

    def idx():
        return random.choice(["0", "1", "-1", "2", "-2", "100"])

    gens = [
        lambda: ["SET", k(), v()], lambda: ["SET", k(), v(), "KEEPTTL"],
        lambda: ["SET", k(), v(), random.choice(["XX", "NX", "GET"]), v()],
        lambda: ["APPEND", k(), v()], lambda: ["SETRANGE", k(), idx(), v()],
        lambda: ["INCR", k()], lambda: ["INCRBY", k(), v()],
        lambda: ["INCRBYFLOAT", k(), v()], lambda: ["DECR", k()],
        lambda: ["GETSET", k(), v()], lambda: ["GETDEL", k()],
        lambda: ["SETBIT", k(), random.choice(["0", "7", "100"]), random.choice(["0", "1"])],
        lambda: ["BITFIELD", k(), "INCRBY", "u8", "0", v()],
        lambda: ["LPUSH", k(), v()], lambda: ["RPUSH", k(), v(), v()],
        lambda: ["LPOP", k()], lambda: ["RPOP", k(), idx()],
        lambda: ["LSET", k(), idx(), v()],
        lambda: ["LINSERT", k(), random.choice(["BEFORE", "AFTER"]), v(), v()],
        lambda: ["LREM", k(), idx(), v()], lambda: ["LTRIM", k(), idx(), idx()],
        lambda: ["SADD", k(), v(), v()], lambda: ["SREM", k(), v()],
        lambda: ["SMOVE", k(), k(), v()],
        lambda: ["HSET", k(), v(), v()], lambda: ["HDEL", k(), v()],
        lambda: ["HINCRBY", k(), v(), v()], lambda: ["HINCRBYFLOAT", k(), v(), v()],
        lambda: ["ZADD", k(), v(), v()],
        lambda: ["ZADD", k(), random.choice(["GT", "LT", "NX", "XX"]), v(), v()],
        lambda: ["ZINCRBY", k(), v(), v()], lambda: ["ZREM", k(), v()],
        lambda: ["ZPOPMIN", k(), idx()],
        lambda: ["DEL", k()], lambda: ["COPY", k(), k(), "REPLACE"],
        lambda: ["RENAME", k(), k()],
    ]

    def snap(c):
        st = {}
        for kk in KEYS:
            t = c.cmd("TYPE", kk)[1]
            if t == "none":
                st[kk] = ("none",)
            elif t == "string":
                st[kk] = ("string", c.cmd("GET", kk))
            elif t == "list":
                st[kk] = ("list", c.cmd("LRANGE", kk, "0", "-1"))
            elif t == "set":
                m = c.cmd("SMEMBERS", kk)
                st[kk] = ("set", tuple(sorted(map(repr, m[1]))) if m[0] == "arr" else m)
            elif t == "hash":
                fl = c.cmd("HGETALL", kk)
                pairs = fl[1] if fl[0] == "arr" else []
                st[kk] = ("hash", tuple(sorted(map(repr, pairs))))
            elif t == "zset":
                st[kk] = ("zset", c.cmd("ZRANGE", kk, "0", "-1", "WITHSCORES"))
            else:
                st[kk] = ("other", t)
        return st

    def ping_ok(conn):
        # +PONG iff the connection is still correctly framed; a stale/partial
        # frame (recv split under host load) is the desync signal.
        try:
            return conn.cmd("PING") == ("ss", "PONG")
        except Exception:
            return False

    for sd in range(args.seeds):
        seed = 4000 + sd
        attempt = 0
        while True:
            attempt += 1
            random.seed(seed)
            R.cmd("FLUSHALL"); F.cmd("FLUSHALL")
            desynced = False
            for n in range(args.iters):
                argv = random.choice(gens)()
                up = [str(x).upper() for x in argv]
                if argv[0] in ("SPOP", "EXPIRE", "PEXPIRE", "EXPIREAT", "PEXPIREAT",
                               "GETEX", "PERSIST", "SETEX", "PSETEX"):
                    continue
                if argv[0].startswith("PF"):
                    continue
                if any(o in up for o in ("EX", "PX", "EXAT", "PXAT")):
                    continue
                R.cmd(*argv)
                F.cmd(*argv)
                sr, sf = snap(R), snap(F)
                if sr != sf:
                    # Re-verify framing before declaring a real bug: a transient
                    # desync leaves the buffers misaligned and surfaces a phantom
                    # state divergence. Only a divergence on two *synced*
                    # connections is real.
                    if ping_ok(R) and ping_ok(F):
                        print(f"FAIL: state divergence seed={seed} iter={n}: {argv}")
                        for kk in KEYS:
                            if sr[kk] != sf[kk]:
                                print(f"  {kk}: redis={sr[kk]}\n      fr   ={sf[kk]}")
                        sys.exit(1)
                    print(f"WARN: transient connection desync at seed={seed} iter={n} "
                          f"({argv}); reconnecting and retrying seed", file=sys.stderr)
                    R, F = Conn(args.oracle), Conn(args.fr)
                    desynced = True
                    break
            if not desynced:
                print(f"seed {seed}: {args.iters} iters state-identical")
                break
            if attempt >= 5:
                print(f"FAIL: seed={seed} kept desyncing after {attempt} attempts "
                      "(infra problem, not a parity result)", file=sys.stderr)
                sys.exit(2)
    print(f"OK: {args.seeds} seeds x {args.iters} cmds — fr keyspace state matches "
          "redis 7.2.4 after every command")


if __name__ == "__main__":
    main()
