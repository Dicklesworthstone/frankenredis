#!/usr/bin/env python3
"""zset_differ.py — seeded randomized differential fuzzer for sorted sets.

Applies the SAME random sequence of ZSET commands (ZADD with NX/XX/GT/LT/CH/INCR,
ZINCRBY, ZRANGE/ZRANGEBYSCORE/ZRANGEBYLEX with exclusive bounds + LIMIT + REV +
WITHSCORES, ZRANGESTORE, ZPOPMIN/MAX, ZMPOP, ZRANK/ZREVRANK WITHSCORE,
ZCOUNT/ZLEXCOUNT, set-algebra ZDIFF/ZINTER/ZUNION[STORE] with WEIGHTS/AGGREGATE,
ZREMRANGEBY*) to fr-server and the vendored redis 7.2.4 oracle, comparing the
reply after every op plus the full `ZRANGE key 0 -1 WITHSCORES` of every key.

Scores cover negatives, floats, and +inf/-inf; float formatting must be exact
(no masking). Exits non-zero on the first divergence, printing seed + op log.

Usage: zset_differ.py [--oracle 16399] [--fr 16400] [--iters 4000] [--seed N]
"""
import argparse
import random
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port))
        self.buf = b""

    def _readline(self):
        while b"\r\n" not in self.buf:
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError("connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _readn(self, n):
        while len(self.buf) < n + 2:
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError("connection closed")
            self.buf += chunk
        data, self.buf = self.buf[:n], self.buf[n + 2:]
        return data

    def _parse(self):
        line = self._readline()
        t, rest = line[:1], line[1:]
        if t == b"+":
            return ("status", rest)
        if t == b"-":
            return ("error", rest)
        if t == b":":
            return ("int", int(rest))
        if t == b"$":
            n = int(rest)
            return ("nil", None) if n < 0 else ("bulk", self._readn(n))
        if t in (b"*", b"%", b"~", b">"):
            n = int(rest)
            if n < 0:
                return ("nil", None)
            if t == b"%":
                n *= 2
            return ("array", [self._parse() for _ in range(n)])
        if t == b",":
            return ("double", rest)
        if t == b"#":
            return ("bool", rest)
        if t == b"_":
            return ("nil", None)
        return ("other", rest)

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, (int, float)):
                a = repr(a) if isinstance(a, float) else str(a)
            if isinstance(a, str):
                a = a.encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self._parse()


def render(node):
    typ, val = node
    if typ == "array":
        return "[" + ",".join(render(x) for x in val) + "]"
    return "%s:%r" % (typ, val)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--iters", type=int, default=4000)
    ap.add_argument("--seed", type=int, default=1234)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    o, f = Conn(args.oracle), Conn(args.fr)
    o.cmd("FLUSHALL")
    f.cmd("FLUSHALL")

    keys = ["z1", "z2", "z3"]
    members = ["a", "b", "c", "d", "e", "f", "g"]
    log = []

    def score():
        return rng.choice(
            ["1", "2", "0", "-1", "1.5", "-2.5", "3.0e2", "+inf", "-inf",
             str(rng.randint(-5, 5)), "%.3f" % rng.uniform(-3, 3)]
        )

    def lexbound():
        m = rng.choice(members)
        return rng.choice(["-", "+", "[" + m, "(" + m])

    def scorebound():
        return rng.choice(
            ["-inf", "+inf", str(rng.randint(-5, 5)), "(" + str(rng.randint(-5, 5)), "1.5"]
        )

    def zadd_args():
        flags = []
        # NX is mutually exclusive with XX/GT/LT; pick a coherent combo.
        mode = rng.choice([[], ["NX"], ["XX"], ["GT"], ["LT"], ["XX", "GT"], ["XX", "LT"]])
        flags += mode
        if rng.random() < 0.4:
            flags.append("CH")
        if rng.random() < 0.25:
            flags.append("INCR")  # INCR → single member only
        out = ["ZADD", rng.choice(keys)] + flags
        if "INCR" in flags:
            out += [score(), rng.choice(members)]
        else:
            for _ in range(rng.randint(1, 3)):
                out += [score(), rng.choice(members)]
        return tuple(out)

    ops = [
        zadd_args,
        lambda: ("ZINCRBY", rng.choice(keys), score(), rng.choice(members)),
        lambda: ("ZREM", rng.choice(keys), rng.choice(members), rng.choice(members)),
        lambda: ("ZSCORE", rng.choice(keys), rng.choice(members)),
        lambda: ("ZMSCORE", rng.choice(keys), rng.choice(members), rng.choice(members)),
        lambda: ("ZCARD", rng.choice(keys)),
        lambda: ("ZRANK", rng.choice(keys), rng.choice(members)) + (("WITHSCORE",) if rng.random() < 0.5 else ()),
        lambda: ("ZREVRANK", rng.choice(keys), rng.choice(members)) + (("WITHSCORE",) if rng.random() < 0.5 else ()),
        lambda: ("ZRANGE", rng.choice(keys), str(rng.randint(-4, 4)), str(rng.randint(-4, 4))) + (("WITHSCORES",) if rng.random() < 0.5 else ()),
        lambda: ("ZRANGE", rng.choice(keys), scorebound(), scorebound(), "BYSCORE") + (("REV",) if rng.random() < 0.3 else ()) + (("LIMIT", str(rng.randint(0, 3)), str(rng.randint(-1, 4))) if rng.random() < 0.5 else ()),
        lambda: ("ZRANGE", rng.choice(keys), lexbound(), lexbound(), "BYLEX") + (("LIMIT", str(rng.randint(0, 3)), str(rng.randint(-1, 4))) if rng.random() < 0.5 else ()),
        lambda: ("ZRANGEBYSCORE", rng.choice(keys), scorebound(), scorebound()) + (("WITHSCORES",) if rng.random() < 0.5 else ()),
        lambda: ("ZREVRANGEBYSCORE", rng.choice(keys), scorebound(), scorebound()),
        lambda: ("ZRANGEBYLEX", rng.choice(keys), lexbound(), lexbound()),
        lambda: ("ZREVRANGEBYLEX", rng.choice(keys), lexbound(), lexbound()),
        lambda: ("ZRANGESTORE", rng.choice(keys), rng.choice(keys), str(rng.randint(-3, 3)), str(rng.randint(-3, 3))),
        lambda: ("ZCOUNT", rng.choice(keys), scorebound(), scorebound()),
        lambda: ("ZLEXCOUNT", rng.choice(keys), lexbound(), lexbound()),
        lambda: ("ZPOPMIN", rng.choice(keys)) + ((str(rng.randint(1, 3)),) if rng.random() < 0.5 else ()),
        lambda: ("ZPOPMAX", rng.choice(keys)) + ((str(rng.randint(1, 3)),) if rng.random() < 0.5 else ()),
        lambda: ("ZMPOP", "2", rng.choice(keys), rng.choice(keys), rng.choice(["MIN", "MAX"])) + (("COUNT", str(rng.randint(1, 3))) if rng.random() < 0.5 else ()),
        lambda: ("ZREMRANGEBYRANK", rng.choice(keys), str(rng.randint(-3, 3)), str(rng.randint(-3, 3))),
        lambda: ("ZREMRANGEBYSCORE", rng.choice(keys), scorebound(), scorebound()),
        lambda: ("ZREMRANGEBYLEX", rng.choice(keys), lexbound(), lexbound()),
        lambda: ("ZDIFF", "2", rng.choice(keys), rng.choice(keys)) + (("WITHSCORES",) if rng.random() < 0.5 else ()),
        lambda: ("ZINTER", "2", rng.choice(keys), rng.choice(keys)) + (("AGGREGATE", rng.choice(["SUM", "MIN", "MAX"])) if rng.random() < 0.5 else ()) + (("WITHSCORES",) if rng.random() < 0.5 else ()),
        lambda: ("ZUNION", "2", rng.choice(keys), rng.choice(keys), "WEIGHTS", str(rng.randint(1, 3)), str(rng.randint(1, 3))) + (("WITHSCORES",) if rng.random() < 0.5 else ()),
        lambda: ("ZUNIONSTORE", rng.choice(keys), "2", rng.choice(keys), rng.choice(keys)) + (("AGGREGATE", rng.choice(["SUM", "MIN", "MAX"])) if rng.random() < 0.5 else ()),
        lambda: ("ZINTERCARD", "2", rng.choice(keys), rng.choice(keys)) + (("LIMIT", str(rng.randint(0, 3))) if rng.random() < 0.5 else ()),
    ]

    lex_cmds = {"ZRANGEBYLEX", "ZREVRANGEBYLEX", "ZLEXCOUNT", "ZREMRANGEBYLEX"}

    def has_unequal_scores(key):
        # A lex range is only well-defined when every member shares one score.
        node = o.cmd("ZRANGE", key, "0", "-1", "WITHSCORES")
        if node[0] != "array":
            return False
        items = node[1]
        scores = {items[i][1] for i in range(1, len(items), 2)}
        return len(scores) > 1

    for it in range(args.iters):
        op = rng.choice(ops)()
        # Skip lex-family ops on a zset with unequal scores: redis resolves the
        # bound with a skiplist-level search (zslFirst/LastInLexRange) that is
        # unspecified on mixed scores and not reproducible by a linear scan
        # (frankenredis-vgkly, WONTFIX). Skipping on both servers keeps them in
        # lockstep — equal-score lex (which IS byte-exact) stays covered. Matches
        # random_reply_differ, which excludes the whole lex family.
        if (op[0] in lex_cmds or (op[0] == "ZRANGE" and "BYLEX" in op)) and has_unequal_scores(op[1]):
            continue
        ro, rf = o.cmd(*op), f.cmd(*op)
        nro, nrf = render(ro), render(rf)
        log.append(" ".join(str(x) for x in op) + "  => O:%s F:%s" % (nro[:50], nrf[:50]))
        diverged = None
        if nro != nrf:
            diverged = ("reply", nro, nrf)
        else:
            for k in keys:
                so = render(o.cmd("ZRANGE", k, "0", "-1", "WITHSCORES"))
                sf = render(f.cmd("ZRANGE", k, "0", "-1", "WITHSCORES"))
                if so != sf:
                    diverged = ("state " + k, so, sf)
                    break
        if diverged:
            kind, vo, vf = diverged
            print("=== DIVERGENCE at iter %d (%s) ===" % (it, kind))
            print("seed=%d" % args.seed)
            print("op: %s" % " ".join(str(x) for x in op))
            print("oracle: %s" % vo[:1200])
            print("fr    : %s" % vf[:1200])
            print("--- op log (last 50) ---")
            for line in log[-50:]:
                print("  " + line)
            sys.exit(1)

    print("OK: %d iters, seed %d — no divergence (fr zset matches redis 7.2.4)" % (args.iters, args.seed))


if __name__ == "__main__":
    main()
