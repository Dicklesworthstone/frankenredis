#!/usr/bin/env python3
"""bitmap_differ.py — seeded randomized differential fuzzer for bitmap/bit ops.

Applies the SAME random sequence of SETBIT/GETBIT/BITCOUNT/BITPOS/BITOP/BITFIELD
(plus SET/SETRANGE/APPEND/GETRANGE to mutate the underlying string) to fr-server
and the vendored redis 7.2.4 oracle, comparing the reply after every op plus the
raw GET of every key. Exercises BITFIELD signed/unsigned widths (i1..i64,
u1..u63), OVERFLOW WRAP/SAT/FAIL, # offset multipliers; BITCOUNT/BITPOS BYTE|BIT
ranges with negative indices; BITOP AND/OR/XOR/NOT with mismatched lengths.

Usage: bitmap_differ.py [--oracle 16399] [--fr 16400] [--iters 4000] [--seed N]
Exits non-zero on the first divergence, printing seed + op log.
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
        if t == b",":
            return ("double", rest)
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
    if typ == "error":
        # Normalize only the error class (first token) — wording can vary, but
        # for bit ops redis messages are stable; compare full text.
        return "error:%r" % val
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

    keys = ["b1", "b2", "b3"]
    log = []

    def off():
        return str(rng.randint(0, 80))

    def bftype():
        signed = rng.random() < 0.5
        width = rng.choice([1, 2, 3, 5, 7, 8, 13, 16, 31, 32, 63, 64])
        if not signed and width == 64:
            width = 63  # u64 is invalid; max unsigned is u63
        return ("i" if signed else "u") + str(width)

    def bfoffset():
        return rng.choice([off(), "#" + str(rng.randint(0, 10))])

    def bfval():
        return str(rng.choice([0, 1, -1, 255, 256, -128, 127, -32768, 32767,
                               rng.randint(-1000, 1000), 2**31, -(2**31)]))

    def rangeargs():
        # optional [start end [BYTE|BIT]]
        if rng.random() < 0.3:
            return ()
        a = [str(rng.randint(-10, 20))]
        if rng.random() < 0.85:
            a.append(str(rng.randint(-10, 20)))
            if rng.random() < 0.6:
                a.append(rng.choice(["BYTE", "BIT"]))
        return tuple(a)

    def bitfield_args():
        out = ["BITFIELD", rng.choice(keys)]
        for _ in range(rng.randint(1, 3)):
            kind = rng.random()
            if rng.random() < 0.4:
                out += ["OVERFLOW", rng.choice(["WRAP", "SAT", "FAIL"])]
            if kind < 0.34:
                out += ["GET", bftype(), bfoffset()]
            elif kind < 0.67:
                out += ["SET", bftype(), bfoffset(), bfval()]
            else:
                out += ["INCRBY", bftype(), bfoffset(), bfval()]
        return tuple(out)

    def randbytes():
        return bytes(rng.randint(0, 255) for _ in range(rng.randint(0, 6)))

    ops = [
        lambda: ("SETBIT", rng.choice(keys), off(), str(rng.randint(0, 1))),
        lambda: ("GETBIT", rng.choice(keys), off()),
        lambda: ("BITCOUNT", rng.choice(keys)) + rangeargs(),
        lambda: ("BITPOS", rng.choice(keys), str(rng.randint(0, 1))) + rangeargs(),
        lambda: ("BITOP", rng.choice(["AND", "OR", "XOR"]), rng.choice(keys), rng.choice(keys), rng.choice(keys)),
        lambda: ("BITOP", "NOT", rng.choice(keys), rng.choice(keys)),
        bitfield_args,
        lambda: ("SET", rng.choice(keys), randbytes()),
        lambda: ("SETRANGE", rng.choice(keys), off(), randbytes()),
        lambda: ("APPEND", rng.choice(keys), randbytes()),
        lambda: ("GETRANGE", rng.choice(keys), str(rng.randint(-8, 12)), str(rng.randint(-8, 12))),
        lambda: ("STRLEN", rng.choice(keys)),
        lambda: ("DEL", rng.choice(keys)),
    ]

    for it in range(args.iters):
        op = rng.choice(ops)()
        ro, rf = o.cmd(*op), f.cmd(*op)
        nro, nrf = render(ro), render(rf)
        opstr = " ".join(repr(x) if isinstance(x, bytes) else str(x) for x in op)
        log.append(opstr + "  => O:%s F:%s" % (nro[:48], nrf[:48]))
        diverged = None
        if nro != nrf:
            diverged = ("reply", nro, nrf)
        else:
            for k in keys:
                so, sf = render(o.cmd("GET", k)), render(f.cmd("GET", k))
                if so != sf:
                    diverged = ("state " + k, so, sf)
                    break
        if diverged:
            kind, vo, vf = diverged
            print("=== DIVERGENCE at iter %d (%s) ===" % (it, kind))
            print("seed=%d" % args.seed)
            print("op: %s" % opstr)
            print("oracle: %s" % vo[:1000])
            print("fr    : %s" % vf[:1000])
            print("--- op log (last 50) ---")
            for line in log[-50:]:
                print("  " + line)
            sys.exit(1)

    print("OK: %d iters, seed %d — no divergence (fr bitmap matches redis 7.2.4)" % (args.iters, args.seed))


if __name__ == "__main__":
    main()
