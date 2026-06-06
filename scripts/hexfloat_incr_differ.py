#!/usr/bin/env python3
"""hexfloat_incr_differ.py — differential gate for C99 hex-float operands in
INCRBYFLOAT / HINCRBYFLOAT (fr vs vendored redis 7.2.4).

redis parses the increment with `strtold`, which accepts C99 hex floats
("0x10"==16, "0x1.8p1"==3, "0xff"==255, "0x1.5p3"==10.5). fr's store used a
decimal-only `parse_long_double`, so these used to error. This sweep feeds both
hand-picked and randomly-generated hex-float operands through INCRBYFLOAT (and
HINCRBYFLOAT) against a fresh key and compares the reply byte-for-byte, plus the
chained-accumulation result so the rounded f80 value matches too.

Usage: hexfloat_incr_differ.py [--oracle 16399] [--fr 16400] [--n 4000] [--seed 1]
Exit 0 if byte-exact, else 1.
"""
import argparse
import random
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
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

    def _parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t in (b"+", b":"):
            return r.decode()
        if t == b"-":
            return "ERR:" + r.decode()
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self._parse()


def handpicked():
    out = []
    # plain hex ints
    for v in ["0x0", "0x1", "0xff", "0x10", "0xFF", "0xABCDEF", "0x7fffffff",
              "0xffffffff", "0x100000000", "0xdeadbeef", "0x1p0", "0x1p4",
              "0x1p-4", "0x1.8p1", "0x1.5p3", "0x1.999999999999ap-4",
              "0xa.bp2", "0x.8p1", "0x1.p3", "0xcp-2", "0x0p0", "0x10e5",
              "0x1.0000000000000p0", "0xfedcba9876543",
              "-0x10", "-0xff", "-0x1.8p1", "+0x10", "+0x1.5p3",
              "0X10", "0Xff", "0x1P4", "0xABp-1"]:
        out.append(v)
    return out


def rand_hex(rng):
    sign = rng.choice(["", "", "-", "+"])
    ndig = rng.randint(1, 13)  # <=13 keeps significand inside u64 with frac digits
    digits = "".join(rng.choice("0123456789abcdefABCDEF") for _ in range(ndig))
    body = digits
    if rng.random() < 0.5:
        fdig = rng.randint(0, 6)
        frac = "".join(rng.choice("0123456789abcdef") for _ in range(fdig))
        body = digits + "." + frac
    expo = ""
    if rng.random() < 0.6:
        p = rng.choice(["p", "P"])
        e = rng.randint(-30, 30)
        expo = f"{p}{e}"
    pref = rng.choice(["0x", "0X"])
    return f"{sign}{pref}{body}{expo}"


def probe(o, f, key_kind, operand, idx):
    """Run one INCRBYFLOAT or HINCRBYFLOAT step on both servers, compare."""
    diffs = []
    if key_kind == "string":
        k = f"hf:s:{idx}"
        o.cmd("DEL", k)
        f.cmd("DEL", k)
        ro = o.cmd("INCRBYFLOAT", k, operand)
        rf = f.cmd("INCRBYFLOAT", k, operand)
        if ro != rf:
            diffs.append(("INCRBYFLOAT", operand, ro, rf))
        # chain a second increment so rounding/accumulation is exercised
        ro2 = o.cmd("INCRBYFLOAT", k, operand)
        rf2 = f.cmd("INCRBYFLOAT", k, operand)
        if ro2 != rf2:
            diffs.append(("INCRBYFLOAT(x2)", operand, ro2, rf2))
    else:
        k = f"hf:h:{idx}"
        o.cmd("DEL", k)
        f.cmd("DEL", k)
        ro = o.cmd("HINCRBYFLOAT", k, "fld", operand)
        rf = f.cmd("HINCRBYFLOAT", k, "fld", operand)
        if ro != rf:
            diffs.append(("HINCRBYFLOAT", operand, ro, rf))
        ro2 = o.cmd("HINCRBYFLOAT", k, "fld", operand)
        rf2 = f.cmd("HINCRBYFLOAT", k, "fld", operand)
        if ro2 != rf2:
            diffs.append(("HINCRBYFLOAT(x2)", operand, ro2, rf2))
    return diffs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--n", type=int, default=4000)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    o, f = Conn(args.oracle), Conn(args.fr)
    for c in (o, f):
        if c.cmd("PING") != "PONG":
            print("server not responding", file=sys.stderr)
            sys.exit(2)

    rng = random.Random(args.seed)
    operands = list(handpicked())
    while len(operands) < args.n:
        operands.append(rand_hex(rng))

    faults = 0
    shown = 0
    for i, operand in enumerate(operands):
        for kind in ("string", "hash"):
            for d in probe(o, f, kind, operand, i):
                faults += 1
                if shown < 30:
                    shown += 1
                    cmd, op, ro, rf = d
                    print(f"DIFF [{cmd}] operand={op!r}")
                    print(f"   oracle: {ro!r}")
                    print(f"   fr    : {rf!r}")

    print(f"--- {len(operands)} operands x2 cmds x2 chained: {faults} divergences ---")
    if faults:
        print("FAIL: hex-float INCRBYFLOAT/HINCRBYFLOAT diverges")
        sys.exit(1)
    print("OK: hex-float INCRBYFLOAT/HINCRBYFLOAT byte-exact vs redis 7.2.4")


if __name__ == "__main__":
    main()
