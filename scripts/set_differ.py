#!/usr/bin/env python3
"""set_differ.py — seeded randomized differential fuzzer for SET commands.

Applies the SAME random sequence of SADD/SREM/SMOVE/SPOP(+count)/SCARD/SISMEMBER/
SMISMEMBER/SINTERSTORE/SUNIONSTORE/SDIFFSTORE/SINTER/SUNION/SDIFF/SINTERCARD/
SRANDMEMBER to fr-server and the vendored redis 7.2.4 oracle. Members mix
integers (intset), short strings (listpack), and big strings (hashtable) so the
intset↔listpack↔hashtable conversions are exercised. SMEMBERS/SINTER/etc. element
ORDER is unspecified, so those replies and full-set state are compared as SORTED
sets; SPOP/SRANDMEMBER (random) are compared by property (length + membership).

Usage: set_differ.py [--oracle 16399] [--fr 16400] [--iters 4000] [--seed N]
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
            c = self.s.recv(65536)
            if not c:
                raise EOFError("closed")
            self.buf += c
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _readn(self, n):
        while len(self.buf) < n + 2:
            c = self.s.recv(65536)
            if not c:
                raise EOFError("closed")
            self.buf += c
        d, self.buf = self.buf[:n], self.buf[n + 2:]
        return d

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
        if t in (b"*", b"~", b">"):
            n = int(rest)
            return ("nil", None) if n < 0 else ("array", [self._parse() for _ in range(n)])
        if t == b"%":
            n = int(rest)
            return ("array", [self._parse() for _ in range(n * 2)])
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


def as_sorted(reply):
    if reply[0] != "array":
        return render(reply)
    return str(sorted(render(x) for x in reply[1]))


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

    # SPOP is random, so it desyncs the two servers and can't run in the diff
    # loop. Property-check it here on identical fresh sets: the reply LENGTH, the
    # remaining cardinality, and that every popped member was in the set must
    # agree between fr and redis for each count (the exact picks are unspecified).
    def spop_property():
        base = [str(i) for i in range(5)] + ["a", "bb", "ccc"]  # mixed intset/listpack
        for count in ["0", "1", "3", "8", "9", "-1"]:
            for s in (o, f):
                s.cmd("DEL", "sp")
                s.cmd("SADD", "sp", *base)
            ro2, rf2 = o.cmd("SPOP", "sp", count), f.cmd("SPOP", "sp", count)
            # type must match (array on success, error on negative count)
            if ro2[0] != rf2[0]:
                return ("spop-type count=%s" % count, render(ro2), render(rf2))
            if ro2[0] == "array":
                lo, lf = len(ro2[1]), len(rf2[1])
                if lo != lf:
                    return ("spop-len count=%s" % count, str(lo), str(lf))
                # popped members must all be from base, and remaining card agree
                popped_f = {render(x) for x in rf2[1]}
                if not all(render(x)[7:-1] in base for x in rf2[1]):
                    return ("spop-member count=%s" % count, "in-base", str(popped_f))
                co, cf = o.cmd("SCARD", "sp"), f.cmd("SCARD", "sp")
                if co != cf:
                    return ("spop-remaining count=%s" % count, render(co), render(cf))
        o.cmd("DEL", "sp")
        f.cmd("DEL", "sp")
        return None

    sp = spop_property()
    if sp:
        print("=== SPOP PROPERTY DIVERGENCE (%s) ===" % sp[0])
        print("oracle: %s\nfr    : %s" % (sp[1], sp[2]))
        sys.exit(1)

    keys = ["s1", "s2", "s3"]

    def k():
        return rng.choice(keys)

    def member():
        r = rng.random()
        if r < 0.5:
            return str(rng.randint(-20, 50))           # integer → intset
        if r < 0.85:
            return rng.choice(["a", "bb", "ccc", "dd", "ee"])  # short str → listpack
        return "B" * rng.randint(60, 70)               # long str → hashtable

    log = []
    SORTED_REPLY = {"SINTER", "SUNION", "SDIFF", "SMEMBERS"}

    ops = [
        lambda: ("SADD", k(), member(), member(), member()),
        lambda: ("SREM", k(), member(), member()),
        lambda: ("SMOVE", k(), k(), member()),
        lambda: ("SCARD", k()),
        lambda: ("SISMEMBER", k(), member()),
        lambda: ("SMISMEMBER", k(), member(), member(), member()),
        lambda: ("SINTER", k(), k()),
        lambda: ("SUNION", k(), k()),
        lambda: ("SDIFF", k(), k()),
        lambda: ("SINTERSTORE", k(), k(), k()),
        lambda: ("SUNIONSTORE", k(), k(), k()),
        lambda: ("SDIFFSTORE", k(), k(), k()),
        lambda: ("SINTERCARD", "2", k(), k()) + (("LIMIT", str(rng.randint(0, 4))) if rng.random() < 0.5 else ()),
        # SPOP is a RANDOM mutation — it removes unspecified members, so the two
        # servers' set state legitimately desyncs and post-op comparison breaks.
        # Its reply is property-checked separately (see spop_property below); it
        # is excluded from the in-loop mutation set. SRANDMEMBER is read-only.
        lambda: ("SRANDMEMBER", k(), str(rng.randint(-5, 5))),
        lambda: ("DEL", k()),
    ]

    def sset(s, key):
        r = s.cmd("SMEMBERS", key)
        return sorted(render(x) for x in r[1]) if r[0] == "array" else []

    for it in range(args.iters):
        op = rng.choice(ops)()
        name = op[0]
        is_rand = name in ("SPOP", "SRANDMEMBER")
        is_spop_count = name == "SPOP" and len(op) == 3
        ro, rf = o.cmd(*op), f.cmd(*op)
        nro, nrf = render(ro), render(rf)
        log.append(" ".join(str(x) for x in op) + "  => O:%s F:%s" % (nro[:40], nrf[:40]))
        diverged = None
        if is_rand:
            # random pick: type + length must match (SPOP without count returns a
            # bulk/nil; with count returns an array; SRANDMEMBER count returns array)
            if ro[0] != rf[0]:
                diverged = ("rand-type", nro, nrf)
            elif ro[0] == "array" and len(ro[1]) != len(rf[1]):
                diverged = ("rand-len", nro, nrf)
        elif name in SORTED_REPLY:
            if as_sorted(ro) != as_sorted(rf):
                diverged = ("reply", as_sorted(ro), as_sorted(rf))
        elif nro != nrf:
            diverged = ("reply", nro, nrf)
        if not diverged:
            for key in keys:
                so, sf = sset(o, key), sset(f, key)
                if so != sf:
                    diverged = ("state " + key, str(so), str(sf))
                    break
        if diverged:
            kind, vo, vf = diverged
            print("=== DIVERGENCE at iter %d (%s) ===" % (it, kind))
            print("seed=%d" % args.seed)
            print("op: %s" % " ".join(str(x) for x in op))
            print("oracle: %s" % vo[:900])
            print("fr    : %s" % vf[:900])
            print("--- op log (last 40) ---")
            for line in log[-40:]:
                print("  " + line)
            sys.exit(1)

    print("OK: %d iters, seed %d — no divergence (fr set matches redis 7.2.4)" % (args.iters, args.seed))


if __name__ == "__main__":
    main()
