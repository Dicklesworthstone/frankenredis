#!/usr/bin/env python3
"""encoding_differ.py — differential fuzzer for OBJECT ENCODING transitions.

Hammers each value type across its conversion thresholds (value-length and
entry-count) and after every mutation compares `OBJECT ENCODING key`, `TYPE
key`, and a type-appropriate full read, fr-server vs vendored redis 7.2.4.
Targets the listpack↔quicklist / listpack↔hashtable / intset↔listpack↔hashtable
/ listpack↔skiplist conversion paths (compiled-default thresholds: hash/zset/
set/list listpack entries 128, value 64; set intset 512).

Both servers MUST run config-less (compiled defaults) so thresholds align.

Usage: encoding_differ.py [--oracle 16399] [--fr 16400] [--iters 3000] [--seed N]
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--iters", type=int, default=3000)
    ap.add_argument("--seed", type=int, default=1234)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    o, f = Conn(args.oracle), Conn(args.fr)
    o.cmd("FLUSHALL")
    f.cmd("FLUSHALL")

    keys = {"s": "string", "l": "list", "h": "hash", "se": "set", "z": "zset"}

    def small():
        return rng.choice(["a", "b", "1", "42", "-7", "xy"])

    def big():
        return "B" * rng.randint(60, 70)  # straddles the 64-byte listpack-value limit

    def intish():
        return str(rng.randint(-1000, 1000))

    def val():
        return rng.choice([small(), small(), intish(), big()])

    def member():
        return rng.choice(["m" + str(rng.randint(0, 20)), small(), big()])

    log = []

    def ops_for(key, typ):
        if typ == "string":
            return [
                ("SET", key, val()),
                ("APPEND", key, small()),
                ("SETRANGE", key, str(rng.randint(0, 70)), small()),
                ("SET", key, intish()),
                ("INCR", key),
            ]
        if typ == "list":
            return [
                ("RPUSH", key, val(), val()),
                ("LPUSH", key, val()),
                ("RPOP", key),
                ("LINSERT", key, "BEFORE", small(), big()),
                ("RPUSH", key, *([small()] * rng.randint(1, 40))),  # push toward count limit
            ]
        if typ == "hash":
            return [
                ("HSET", key, member(), val()),
                ("HSET", key, member(), big()),
                ("HDEL", key, member()),
                ("HSET", key, *sum(([("g%d" % i), small()] for i in range(rng.randint(1, 30))), [])),
            ]
        if typ == "set":
            return [
                ("SADD", key, intish(), intish()),
                ("SADD", key, member()),  # non-int member → intset breaks
                ("SADD", key, big()),
                ("SREM", key, intish()),
                ("SADD", key, *([str(i) for i in range(rng.randint(1, 40))])),
            ]
        # zset
        return [
            ("ZADD", key, str(rng.randint(-5, 5)), member()),
            ("ZADD", key, "1", big()),
            ("ZREM", key, member()),
            ("ZADD", key, *sum(([str(i), "zm%d" % i] for i in range(rng.randint(1, 30))), [])),
        ]

    for it in range(args.iters):
        key = rng.choice(list(keys))
        typ = keys[key]
        op = tuple(str(x) for x in rng.choice(ops_for(key, typ)))
        ro, rf = o.cmd(*op), f.cmd(*op)
        nro, nrf = render(ro), render(rf)
        log.append(" ".join(op)[:70] + "  => O:%s F:%s" % (nro[:30], nrf[:30]))
        diverged = None
        if nro != nrf:
            diverged = ("reply", nro, nrf)
        else:
            content_cmd = {
                "string": ("GET",),
                "list": ("LRANGE", None, "0", "-1"),
                "hash": ("HGETALL",),
                "set": ("SMEMBERS",),
                "zset": ("ZRANGE", None, "0", "-1", "WITHSCORES"),
            }
            def members(reply):
                return sorted(render(x) for x in reply[1]) if reply[0] == "array" else render(reply)
            for kk in keys:
                # content first — an uncaught content divergence (same length,
                # different elements) otherwise only shows up as an encoding diff.
                cc = content_cmd[keys[kk]]
                full = tuple(kk if a is None else a for a in cc)
                ao, af = o.cmd(*full), f.cmd(*full)
                if keys[kk] == "set":  # SMEMBERS order is unspecified; sort
                    co, cf = str(members(ao)), str(members(af))
                else:
                    co, cf = render(ao), render(af)
                if co != cf:
                    diverged = ("content " + kk, co, cf)
                    break
                # rc49s FIXED: list listpack→quicklist transition is now decided
                # at ADD time on raw element lengths (sticky, order-faithful),
                # matching redis t_list.c — the list-key encoding check is back on.
                eo, ef = render(o.cmd("OBJECT", "ENCODING", kk)), render(f.cmd("OBJECT", "ENCODING", kk))
                if eo != ef:
                    diverged = ("encoding " + kk, eo, ef)
                    break
                to, tf = render(o.cmd("TYPE", kk)), render(f.cmd("TYPE", kk))
                if to != tf:
                    diverged = ("type " + kk, to, tf)
                    break
        if diverged:
            kind, vo, vf = diverged
            print("=== DIVERGENCE at iter %d (%s) ===" % (it, kind))
            print("seed=%d" % args.seed)
            print("op: %s" % " ".join(op))
            print("oracle: %s" % vo[:600])
            print("fr    : %s" % vf[:600])
            print("--- op log (last 40) ---")
            for line in log[-40:]:
                print("  " + line)
            sys.exit(1)

    print("OK: %d iters, seed %d — no divergence (fr encoding matches redis 7.2.4)" % (args.iters, args.seed))


if __name__ == "__main__":
    main()
