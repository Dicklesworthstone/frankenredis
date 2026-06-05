#!/usr/bin/env python3
"""list_differ.py — seeded randomized differential fuzzer for LIST commands.

Applies the SAME random sequence of LPUSH/RPUSH/LPOP/RPOP (with COUNT)/LINSERT
BEFORE|AFTER/LSET/LREM (negative/positive/zero count)/LRANGE/LINDEX/LLEN/LTRIM/
LMOVE/RPOPLPUSH/LMPOP/LPOS (RANK + COUNT + MAXLEN) to fr-server and the vendored
redis 7.2.4 oracle, comparing the reply after every op plus the full
`LRANGE key 0 -1` of every key. Exits non-zero on the first divergence.

Usage: list_differ.py [--oracle 16399] [--fr 16400] [--iters 4000] [--seed N]
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
                raise EOFError("closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _readn(self, n):
        while len(self.buf) < n + 2:
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError("closed")
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
        if t == b"_":
            return ("nil", None)
        return ("other", rest)

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, int):
                a = str(a)
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

    keys = ["l1", "l2", "l3"]
    # Small element pool so LREM/LPOS/LINSERT hit existing values often.
    elems = ["a", "b", "c", "d", "x", "y"]

    def k():
        return rng.choice(keys)

    def e():
        return rng.choice(elems)

    def idx():
        return str(rng.randint(-6, 6))

    def cnt():
        return str(rng.randint(-4, 4))

    log = []

    ops = [
        lambda: ("LPUSH", k(), e(), e()),
        lambda: ("RPUSH", k(), e(), e()),
        lambda: ("LPUSHX", k(), e()),
        lambda: ("RPUSHX", k(), e()),
        lambda: ("LPOP", k()) + ((str(rng.randint(0, 3)),) if rng.random() < 0.5 else ()),
        lambda: ("RPOP", k()) + ((str(rng.randint(0, 3)),) if rng.random() < 0.5 else ()),
        lambda: ("LLEN", k()),
        lambda: ("LINDEX", k(), idx()),
        lambda: ("LRANGE", k(), idx(), idx()),
        lambda: ("LSET", k(), idx(), e()),
        lambda: ("LINSERT", k(), rng.choice(["BEFORE", "AFTER"]), e(), e()),
        lambda: ("LREM", k(), cnt(), e()),
        lambda: ("LTRIM", k(), idx(), idx()),
        lambda: ("LMOVE", k(), k(), rng.choice(["LEFT", "RIGHT"]), rng.choice(["LEFT", "RIGHT"])),
        lambda: ("RPOPLPUSH", k(), k()),
        lambda: ("LPOS", k(), e()) + (("RANK", str(rng.choice([-3, -1, 1, 2]))) if rng.random() < 0.6 else ())
                 + (("COUNT", str(rng.randint(0, 3))) if rng.random() < 0.6 else ())
                 + (("MAXLEN", str(rng.randint(0, 4))) if rng.random() < 0.4 else ()),
        lambda: ("LMPOP", "2", k(), k(), rng.choice(["LEFT", "RIGHT"]))
                 + (("COUNT", str(rng.randint(1, 3))) if rng.random() < 0.5 else ()),
        lambda: ("DEL", k()),
    ]

    for it in range(args.iters):
        op = rng.choice(ops)()
        ro, rf = o.cmd(*op), f.cmd(*op)
        nro, nrf = render(ro), render(rf)
        log.append(" ".join(str(x) for x in op) + "  => O:%s F:%s" % (nro[:48], nrf[:48]))
        diverged = None
        if nro != nrf:
            diverged = ("reply", nro, nrf)
        else:
            for key in keys:
                so, sf = render(o.cmd("LRANGE", key, "0", "-1")), render(f.cmd("LRANGE", key, "0", "-1"))
                if so != sf:
                    diverged = ("state " + key, so, sf)
                    break
        if diverged:
            kind, vo, vf = diverged
            print("=== DIVERGENCE at iter %d (%s) ===" % (it, kind))
            print("seed=%d" % args.seed)
            print("op: %s" % " ".join(str(x) for x in op))
            print("oracle: %s" % vo[:1000])
            print("fr    : %s" % vf[:1000])
            print("--- op log (last 50) ---")
            for line in log[-50:]:
                print("  " + line)
            sys.exit(1)

    print("OK: %d iters, seed %d — no divergence (fr list matches redis 7.2.4)" % (args.iters, args.seed))


if __name__ == "__main__":
    main()
