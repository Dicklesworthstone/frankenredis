#!/usr/bin/env python3
"""stream_xinfo_differ.py — seeded randomized stream-sequence differential fuzzer.

Applies the SAME random sequence of stream commands (XADD/XDEL/XTRIM/XSETID/
XGROUP/XREADGROUP/XACK/…) to fr-server and the vendored redis 7.2.4 oracle, and
after every op compares the command reply plus `XINFO STREAM <k> FULL` and
`XINFO GROUPS <k>` for divergence. Targets the ren6y residual: XINFO GROUPS
entries-read/lag drift after mixed XADD/XDEL/XTRIM MINID/XACK/XREADGROUP.

Volatile fields (epoch-ms timestamps: seen-time, active-time, delivery-time,
idle, inactive, and any value >= 1e12) are masked so only semantic state is
compared. Explicit IDs only (no '*') so replies are deterministic.

Usage: stream_xinfo_differ.py [--oracle 16399] [--fr 16400] [--iters 4000] [--seed N]
Exits non-zero on the first divergence, printing the full op log + seed.
"""
import argparse
import random
import socket
import sys

VOLATILE_KEYS = {
    b"seen-time", b"active-time", b"delivery-time", b"idle", b"inactive",
}


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
            if n < 0:
                return ("nil", None)
            return ("bulk", self._readn(n))
        if t == b"*" or t == b"%" or t == b"~" or t == b">":
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
        # RESP3 big number / verbatim fallbacks
        return ("other", rest)

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, int):
                a = str(a).encode()
            elif isinstance(a, str):
                a = a.encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self._parse()


def normalize(node):
    """Recursively render a parsed reply to a normalized comparable form,
    masking volatile epoch-ms values that follow a volatile field name."""
    typ, val = node
    if typ == "array":
        items = val
        rendered = []
        i = 0
        while i < len(items):
            cur = items[i]
            rendered.append(normalize(cur))
            # mask the value following a volatile key name
            if cur[0] in ("bulk", "status") and cur[1] in VOLATILE_KEYS and i + 1 < len(items):
                rendered.append("<MASKED>")
                i += 2
                continue
            i += 1
        return "[" + ",".join(rendered) + "]"
    if typ == "int":
        # epoch-ms timestamps leak through as bare ints in some shapes
        if val >= 1_000_000_000_000:
            return "<MASKED-INT>"
        return "i%d" % val
    if typ == "nil":
        return "nil"
    if typ in ("bulk", "status", "error", "double", "bool", "other"):
        return "%s:%r" % (typ, val)
    return repr(node)


def both(o, f, *args):
    ro = o.cmd(*args)
    rf = f.cmd(*args)
    return ro, rf


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--iters", type=int, default=4000)
    ap.add_argument("--seed", type=int, default=1234)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    o = Conn(args.oracle)
    f = Conn(args.fr)
    o.cmd("FLUSHALL")
    f.cmd("FLUSHALL")

    keys = ["s1", "s2"]
    groups = ["g1", "g2"]
    consumers = ["c1", "c2"]
    log = []

    def rid():
        return "%d-%d" % (rng.randint(1, 25), rng.randint(0, 3))

    def check_divergence(label):
        for k in keys:
            for info in (("XINFO", "STREAM", k, "FULL", "COUNT", "0"),
                         ("XINFO", "GROUPS", k)):
                ro = o.cmd(*info)
                rf = f.cmd(*info)
                no, nf = normalize(ro), normalize(rf)
                if no != nf:
                    print("=== DIVERGENCE after %s ===" % label)
                    print("seed=%d" % args.seed)
                    print("probe: %s" % " ".join(info))
                    print("oracle: %s" % no[:1500])
                    print("fr    : %s" % nf[:1500])
                    print("--- op log (last 60) ---")
                    for line in log[-60:]:
                        print("  " + line)
                    return True
        return False

    ops = [
        lambda: ("XADD", rng.choice(keys), rid(), "f", "v%d" % rng.randint(0, 9)),
        lambda: ("XADD", rng.choice(keys), rid(), "f", str(rng.randint(0, 99)), "g", str(rng.randint(0, 99))),
        lambda: ("XDEL", rng.choice(keys), rid()),
        lambda: ("XTRIM", rng.choice(keys), "MAXLEN", str(rng.randint(0, 6))),
        lambda: ("XTRIM", rng.choice(keys), "MINID", str(rng.randint(1, 25))),
        lambda: ("XSETID", rng.choice(keys), rid()),
        lambda: ("XSETID", rng.choice(keys), rid(), "ENTRIESADDED", str(rng.randint(0, 30)), "MAXDELETEDID", rid()),
        lambda: ("XGROUP", "CREATE", rng.choice(keys), rng.choice(groups), rng.choice(["0", "$", rid()]), "MKSTREAM"),
        lambda: ("XGROUP", "SETID", rng.choice(keys), rng.choice(groups), rng.choice(["0", "$", rid()])),
        lambda: ("XGROUP", "CREATECONSUMER", rng.choice(keys), rng.choice(groups), rng.choice(consumers)),
        lambda: ("XGROUP", "DELCONSUMER", rng.choice(keys), rng.choice(groups), rng.choice(consumers)),
        lambda: ("XREADGROUP", "GROUP", rng.choice(groups), rng.choice(consumers), "COUNT", str(rng.randint(1, 4)), "STREAMS", rng.choice(keys), rng.choice([">", "0", rid()])),
        lambda: ("XACK", rng.choice(keys), rng.choice(groups), rid()),
        lambda: ("XADD", rng.choice(keys), "NOMKSTREAM", rid(), "f", "v"),
    ]

    for it in range(args.iters):
        op = rng.choice(ops)()
        ro, rf = both(o, f, *op)
        no, nf = normalize(ro), normalize(rf)
        log.append(" ".join(str(x) for x in op) + "  => O:%s F:%s" % (no[:60], nf[:60]))
        if no != nf:
            print("=== REPLY DIVERGENCE at iter %d ===" % it)
            print("seed=%d" % args.seed)
            print("op: %s" % " ".join(str(x) for x in op))
            print("oracle: %s" % no[:1500])
            print("fr    : %s" % nf[:1500])
            print("--- op log (last 60) ---")
            for line in log[-60:]:
                print("  " + line)
            sys.exit(1)
        if check_divergence("iter %d: %s" % (it, " ".join(str(x) for x in op))):
            sys.exit(1)

    print("OK: %d iters, seed %d — no divergence (fr matches redis 7.2.4)" % (args.iters, args.seed))


if __name__ == "__main__":
    main()
