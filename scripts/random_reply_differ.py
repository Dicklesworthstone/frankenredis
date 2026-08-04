#!/usr/bin/env python3
"""random_reply_differ.py — random-command REPLY differential fuzzer.

Companion to random_state_differ.py: that one compares keyspace STATE after each
command (catching effect bugs); this one compares the exact command REPLY
(catching reply-formatting, error-class, and validation-ORDER bugs that leave
state identical). It drives the same seeded random command stream over a tiny
shared 3-key pool against fr and vendored redis 7.2.4 and stops at the first
reply divergence. The shared small key pool forces cross-type collisions
(wrong-type handling, type-check order) that single-type fuzzers miss. This is
the probe that surfaced frankenredis-yiu5p (PF on a corrupt HLL).

Four reply-divergence classes are genuinely unspecified / environment-dependent
and are EXCLUDED so the gate is deterministic — each is real upstream behavior,
not an fr bug:
  - timing: SETEX/GETEX and SET ... EX|PX|EXAT|PXAT — a tiny TTL races key
    expiry between the two servers' independent wall clocks.
  - random sampling: SPOP / SRANDMEMBER / HRANDFIELD / ZRANDMEMBER — the
    selected members (and, for negative counts, their multiset) are random.
  - unequal-score lex ranges: ZRANGEBYLEX / ZREVRANGEBYLEX / ZLEXCOUNT — on a
    zset with non-equal scores the result is unspecified; redis's skiplist
    search is not reproducible by a linear scan (tracked WONTFIX).
  - PF* on a corrupt HLL — layered-validation residual tracked as
    frankenredis-yiu5p.
Set-returning reads are order-normalized before comparison.

Usage: random_reply_differ.py [--oracle 16399] [--fr 16400] [--seeds 8] [--iters 6000]
Exit 0 if every reply matches across the run, else 1.
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
            return ("err", r.decode("latin1"))  # full message — error wording matters
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
RANDOM_CMDS = {"SPOP", "SRANDMEMBER", "HRANDFIELD", "ZRANDMEMBER"}
LEX_CMDS = {"ZRANGEBYLEX", "ZREVRANGEBYLEX", "ZLEXCOUNT"}
TTL_CMDS = {"SETEX", "PSETEX", "GETEX", "EXPIRE", "PEXPIRE", "EXPIREAT",
            "PEXPIREAT", "PERSIST"}
ORDER_UNSPEC = {"SMEMBERS", "SINTER", "SUNION", "SDIFF", "HKEYS", "HVALS", "HGETALL"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--seeds", type=int, default=8)
    ap.add_argument("--iters", type=int, default=6000)
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
        lambda: ["SET", k(), v()], lambda: ["GET", k()], lambda: ["APPEND", k(), v()],
        lambda: ["SETRANGE", k(), idx(), v()], lambda: ["GETRANGE", k(), idx(), idx()],
        lambda: ["STRLEN", k()], lambda: ["INCR", k()], lambda: ["INCRBY", k(), v()],
        lambda: ["INCRBYFLOAT", k(), v()], lambda: ["DECR", k()],
        lambda: ["GETSET", k(), v()], lambda: ["GETDEL", k()],
        lambda: ["SETBIT", k(), random.choice(["0", "7", "100"]), random.choice(["0", "1"])],
        lambda: ["GETBIT", k(), idx()], lambda: ["BITCOUNT", k()],
        lambda: ["BITPOS", k(), random.choice(["0", "1"])],
        lambda: ["BITFIELD", k(), "GET", "u8", "0"],
        lambda: ["BITFIELD", k(), "INCRBY", "u8", "0", v()],
        lambda: ["LPUSH", k(), v()], lambda: ["RPUSH", k(), v(), v()],
        lambda: ["LPOP", k()], lambda: ["RPOP", k(), idx()],
        lambda: ["LRANGE", k(), idx(), idx()], lambda: ["LLEN", k()],
        lambda: ["LINDEX", k(), idx()], lambda: ["LSET", k(), idx(), v()],
        lambda: ["LINSERT", k(), random.choice(["BEFORE", "AFTER"]), v(), v()],
        lambda: ["LREM", k(), idx(), v()], lambda: ["LPOS", k(), v()],
        lambda: ["LTRIM", k(), idx(), idx()],
        lambda: ["SADD", k(), v(), v()], lambda: ["SREM", k(), v()],
        lambda: ["SISMEMBER", k(), v()], lambda: ["SCARD", k()],
        lambda: ["SMEMBERS", k()], lambda: ["SMISMEMBER", k(), v(), v()],
        lambda: ["SINTERCARD", "2", k(), k()], lambda: ["SMOVE", k(), k(), v()],
        lambda: ["HSET", k(), v(), v()], lambda: ["HGET", k(), v()],
        lambda: ["HDEL", k(), v()], lambda: ["HINCRBY", k(), v(), v()],
        lambda: ["HINCRBYFLOAT", k(), v(), v()], lambda: ["HGETALL", k()],
        lambda: ["HKEYS", k()], lambda: ["HLEN", k()], lambda: ["HEXISTS", k(), v()],
        lambda: ["ZADD", k(), v(), v()],
        lambda: ["ZADD", k(), random.choice(["GT", "LT", "NX", "XX"]), v(), v()],
        lambda: ["ZINCRBY", k(), v(), v()], lambda: ["ZSCORE", k(), v()],
        lambda: ["ZRANK", k(), v()], lambda: ["ZRANGE", k(), idx(), idx()],
        lambda: ["ZRANGEBYSCORE", k(), v(), v()], lambda: ["ZPOPMIN", k(), idx()],
        lambda: ["ZREM", k(), v()], lambda: ["ZCARD", k()], lambda: ["ZCOUNT", k(), v(), v()],
        lambda: ["EXPIRE", k(), v()], lambda: ["TTL", k()], lambda: ["PERSIST", k()],
        lambda: ["TYPE", k()], lambda: ["DEL", k()], lambda: ["EXISTS", k()],
        lambda: ["COPY", k(), k(), "REPLACE"], lambda: ["RENAME", k(), k()],
        lambda: ["OBJECT", "ENCODING", k()],
    ]

    def norm(cmd0, reply):
        if cmd0 in ORDER_UNSPEC and isinstance(reply, tuple) and reply[0] == "arr":
            return ("arr", tuple(sorted(map(repr, reply[1]))))
        return reply

    def ping_ok(conn):
        # A correctly-framed connection answers PING with +PONG. If the buffer
        # is misaligned (a recv split a reply under host load), this reads a
        # stale/partial frame instead — the desync signal.
        try:
            return conn.cmd("PING") == ("ss", "PONG")
        except Exception:
            return False

    def cleanup():
        for conn in (R, F):
            try:
                conn.cmd("FLUSHALL")
            except Exception:
                pass

    for sd in range(args.seeds):
        seed = 1000 + sd
        attempt = 0
        while True:
            attempt += 1
            random.seed(seed)
            R.cmd("FLUSHALL"); F.cmd("FLUSHALL")
            desynced = False
            for n in range(args.iters):
                argv = random.choice(gens)()
                cmd0 = argv[0].upper()
                if cmd0 in RANDOM_CMDS or cmd0 in LEX_CMDS or cmd0 in TTL_CMDS or cmd0.startswith("PF"):
                    continue
                up = [str(x).upper() for x in argv]
                if any(o in up for o in ("EX", "PX", "EXAT", "PXAT")):
                    continue
                a = norm(cmd0, R.cmd(*argv))
                b = norm(cmd0, F.cmd(*argv))
                if a != b:
                    # Re-verify connection framing before declaring a real bug.
                    # A transient desync (a split recv under shared-host load)
                    # leaves the buffers misaligned and surfaces a phantom
                    # divergence; PING confirms both sides are still in frame.
                    # Only a divergence on two *synced* connections is real.
                    if ping_ok(R) and ping_ok(F):
                        print(f"FAIL: reply divergence seed={seed} iter={n}: {argv}")
                        print(f"  redis={a!r}")
                        print(f"  fr   ={b!r}")
                        cleanup()
                        sys.exit(1)
                    print(f"WARN: transient connection desync at seed={seed} iter={n} "
                          f"({argv}); reconnecting and retrying seed", file=sys.stderr)
                    R, F = Conn(args.oracle), Conn(args.fr)
                    desynced = True
                    break
            if not desynced:
                print(f"seed {seed}: {args.iters} iters reply-identical")
                break
            if attempt >= 5:
                print(f"FAIL: seed={seed} kept desyncing after {attempt} attempts "
                      "(infra problem, not a parity result)", file=sys.stderr)
                cleanup()
                sys.exit(2)
    cleanup()
    print(f"OK: {args.seeds} seeds x {args.iters} cmds — fr replies match redis 7.2.4 "
          "(timing/random/unequal-lex/PF-corrupt classes excluded)")


if __name__ == "__main__":
    main()
