#!/usr/bin/env python3
"""edge_sweep_differ.py — deterministic command-edge differential sweep vs
vendored redis 7.2.4. Each test is a literal command sequence run against a
fresh FLUSHALL'd keyspace on both servers; replies compared byte-for-byte.

Targets less-probed corners: LMPOP/ZMPOP/SMISMEMBER, LPOS RANK/COUNT, OBJECT
ENCODING transitions, GETDEL/GETEX, SETRANGE/GETRANGE padding, COPY, SINTERCARD,
TYPE, OBJECT REFCOUNT/IDLETIME, BITCOUNT/BITPOS BYTE|BIT, EXPIRE flags, ZADD GT/LT.

Usage: edge_sweep_differ.py [--oracle 16399] [--fr 16400]
Exit 0 if byte-exact, else 1.
"""
import argparse
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(2.0)
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
        if t in (b"+", b":", b",", b"#", b"("):
            return l.decode("latin1")
        if t == b"-":
            return "ERR:" + r.decode("latin1")
        if t in (b"$", b"="):
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t in (b"*", b"~", b">"):
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        if t == b"%":
            n = int(r)
            return ["MAP"] + [self.parse() for _ in range(2 * n)]
        if t == b"_":
            return None
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


# Each entry: a list of command-arg-tuples. Only the LAST reply is compared,
# unless the command string starts with "CHECK:" meaning compare that reply too.
SCENARIOS = [
    # --- LMPOP / ZMPOP multi-key pops ---
    [("RPUSH", "l1", "a", "b", "c"), ("LMPOP", "2", "l1", "l2", "LEFT")],
    [("RPUSH", "l1", "a", "b", "c"), ("LMPOP", "2", "l1", "l2", "LEFT", "COUNT", "2")],
    [("RPUSH", "l2", "x"), ("LMPOP", "2", "l1", "l2", "RIGHT", "COUNT", "5")],
    [("LMPOP", "2", "nope1", "nope2", "LEFT")],
    [("LMPOP", "1", "k", "BADDIR")],
    [("ZADD", "z1", "1", "a", "2", "b"), ("ZMPOP", "2", "z1", "z2", "MIN")],
    [("ZADD", "z1", "1", "a", "2", "b", "3", "c"), ("ZMPOP", "2", "z1", "z2", "MAX", "COUNT", "2")],
    [("ZMPOP", "2", "no1", "no2", "MIN")],
    # --- SMISMEMBER ---
    [("SADD", "s", "a", "b", "c"), ("SMISMEMBER", "s", "a", "x", "c")],
    [("SMISMEMBER", "nope", "a", "b")],
    # --- LPOS ---
    [("RPUSH", "L", "a", "b", "c", "a", "b", "c", "a"), ("LPOS", "L", "a")],
    [("RPUSH", "L", "a", "b", "c", "a", "b", "c", "a"), ("LPOS", "L", "a", "RANK", "-1")],
    [("RPUSH", "L", "a", "b", "c", "a", "b", "c", "a"), ("LPOS", "L", "a", "COUNT", "0")],
    [("RPUSH", "L", "a", "b", "c", "a", "b", "c", "a"), ("LPOS", "L", "a", "RANK", "-1", "COUNT", "2")],
    [("RPUSH", "L", "a", "b", "c"), ("LPOS", "L", "z", "COUNT", "0")],
    [("RPUSH", "L", "a", "b", "c"), ("LPOS", "L", "z")],
    [("RPUSH", "L", "a", "b", "c", "a"), ("LPOS", "L", "a", "RANK", "0")],
    # --- OBJECT ENCODING transitions ---
    [("RPUSH", "e", "a"), ("OBJECT", "ENCODING", "e")],
    [("RPUSH", "e", *([str(i) for i in range(200)])), ("OBJECT", "ENCODING", "e")],
    [("SADD", "e", "1", "2", "3"), ("OBJECT", "ENCODING", "e")],
    [("SADD", "e", "a", "b", "c"), ("OBJECT", "ENCODING", "e")],
    [("SADD", "e", *([str(i) for i in range(600)])), ("OBJECT", "ENCODING", "e")],
    [("SADD", "e", *(["m" + str(i) for i in range(200)])), ("OBJECT", "ENCODING", "e")],
    [("HSET", "e", "f", "v"), ("OBJECT", "ENCODING", "e")],
    [("HSET", "e", *(sum([["f" + str(i), str(i)] for i in range(200)], []))), ("OBJECT", "ENCODING", "e")],
    [("ZADD", "e", "1", "a"), ("OBJECT", "ENCODING", "e")],
    [("ZADD", "e", *(sum([[str(i), "m" + str(i)] for i in range(200)], []))), ("OBJECT", "ENCODING", "e")],
    [("SET", "e", "12345"), ("OBJECT", "ENCODING", "e")],
    [("SET", "e", "hello"), ("OBJECT", "ENCODING", "e")],
    [("SET", "e", "x" * 50), ("OBJECT", "ENCODING", "e")],
    [("SET", "e", "3.14"), ("OBJECT", "ENCODING", "e")],
    [("SET", "e", "12345"), ("APPEND", "e", "6"), ("OBJECT", "ENCODING", "e")],
    [("SET", "e", "12345678901234567890123456789012345678901234"), ("OBJECT", "ENCODING", "e")],
    # large element forces listpack->quicklist on a list
    [("RPUSH", "e", "x" * 100), ("OBJECT", "ENCODING", "e")],
    [("HSET", "e", "f", "x" * 100), ("OBJECT", "ENCODING", "e")],
    [("ZADD", "e", "1", "x" * 100), ("OBJECT", "ENCODING", "e")],
    [("SADD", "e", "x" * 100), ("OBJECT", "ENCODING", "e")],
    # --- GETDEL / GETEX ---
    [("SET", "g", "v"), ("GETDEL", "g")],
    [("SET", "g", "v"), ("GETDEL", "g"), ("EXISTS", "g")],
    [("GETDEL", "nope")],
    [("SET", "g", "v"), ("GETEX", "g", "EX", "100"), ("TTL", "g")],
    [("SET", "g", "v", "EX", "100"), ("GETEX", "g", "PERSIST"), ("TTL", "g")],
    [("SET", "g", "v"), ("GETEX", "g"), ("TTL", "g")],
    [("SET", "g", "v", "EX", "100"), ("GETEX", "g"), ("TTL", "g")],
    # --- SETRANGE / GETRANGE padding ---
    [("SETRANGE", "sr", "5", "hello"), ("GET", "sr")],
    [("SET", "sr", "Hello World"), ("SETRANGE", "sr", "6", "Redis"), ("GET", "sr")],
    [("SETRANGE", "sr", "0", ""), ("EXISTS", "sr")],
    [("SET", "sr", "Hello World"), ("GETRANGE", "sr", "0", "-1")],
    [("SET", "sr", "Hello World"), ("GETRANGE", "sr", "-5", "-1")],
    [("SET", "sr", "Hello World"), ("GETRANGE", "sr", "-100", "-200")],
    [("SET", "sr", "Hello"), ("GETRANGE", "sr", "10", "20")],
    # --- COPY ---
    [("SET", "src", "v"), ("COPY", "src", "dst"), ("GET", "dst")],
    [("SET", "src", "v"), ("SET", "dst", "old"), ("COPY", "src", "dst")],
    [("SET", "src", "v"), ("SET", "dst", "old"), ("COPY", "src", "dst", "REPLACE"), ("GET", "dst")],
    [("SET", "src", "v", "EX", "100"), ("COPY", "src", "dst"), ("TTL", "dst")],
    # --- SINTERCARD ---
    [("SADD", "a", "1", "2", "3", "4"), ("SADD", "b", "2", "3", "4", "5"), ("SINTERCARD", "2", "a", "b")],
    [("SADD", "a", "1", "2", "3", "4"), ("SADD", "b", "2", "3", "4", "5"), ("SINTERCARD", "2", "a", "b", "LIMIT", "2")],
    [("SINTERCARD", "2", "a", "b", "LIMIT", "0")],
    # --- BITCOUNT / BITPOS with BYTE|BIT ---
    [("SET", "bc", "foobar"), ("BITCOUNT", "bc")],
    [("SET", "bc", "foobar"), ("BITCOUNT", "bc", "1", "1")],
    [("SET", "bc", "foobar"), ("BITCOUNT", "bc", "0", "0", "BIT")],
    [("SET", "bc", "foobar"), ("BITCOUNT", "bc", "5", "30", "BIT")],
    [("SET", "bc", "\x00\xff\xf0"), ("BITPOS", "bc", "1", "0", "-1", "BIT")],
    [("SET", "bc", "\x00\xff\xf0"), ("BITPOS", "bc", "1", "2", "-1", "BYTE")],
    # --- TYPE ---
    [("RPUSH", "t", "a"), ("TYPE", "t")],
    [("XADD", "t", "1-1", "f", "v"), ("TYPE", "t")],  # explicit id (auto-id is wall-clock)
    [("TYPE", "nope")],
    # --- OBJECT REFCOUNT / IDLETIME / FREQ ---
    [("SET", "o", "100"), ("OBJECT", "REFCOUNT", "o")],
    # non-shared value so IDLETIME is ~0 on both (shared ints carry server-uptime idle).
    [("SET", "o", "a-non-shared-string-value"), ("OBJECT", "IDLETIME", "o")],
    [("OBJECT", "REFCOUNT", "nope")],
    [("OBJECT", "HELP")],
    # --- EXPIRE flags ---
    [("SET", "x", "v"), ("EXPIRE", "x", "100", "NX"), ("TTL", "x")],
    [("SET", "x", "v", "EX", "100"), ("EXPIRE", "x", "200", "NX")],
    [("SET", "x", "v", "EX", "100"), ("EXPIRE", "x", "200", "GT"), ("TTL", "x")],
    [("SET", "x", "v", "EX", "100"), ("EXPIRE", "x", "50", "GT")],
    [("SET", "x", "v", "EX", "100"), ("EXPIRE", "x", "50", "LT"), ("TTL", "x")],
    [("SET", "x", "v"), ("EXPIRE", "x", "100", "XX")],
    # --- ZADD GT / LT / NX / XX ---
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "GT", "3", "m"), ("ZSCORE", "z", "m")],
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "GT", "10", "m"), ("ZSCORE", "z", "m")],
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "LT", "3", "m"), ("ZSCORE", "z", "m")],
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "GT", "CH", "10", "m")],
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "NX", "GT", "10", "m")],
    [("ZADD", "z", "XX", "GT", "10", "m")],
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "INCR", "3", "m")],
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "GT", "INCR", "-3", "m")],
    # --- SETEX / GETSET / INCR edge ---
    [("SET", "n", "9223372036854775807"), ("INCR", "n")],
    [("SET", "n", "abc"), ("INCR", "n")],
    [("SET", "n", "3.0e3"), ("INCR", "n")],
    [("SET", "n", "  11"), ("INCR", "n")],
    [("INCRBY", "n", "9223372036854775808")],
    # --- HRANDFIELD / SRANDMEMBER / ZRANDMEMBER counts (deterministic for full set) ---
    [("HSET", "h", "a", "1"), ("HRANDFIELD", "h", "-5", "WITHVALUES")],
    [("SADD", "s", "only"), ("SRANDMEMBER", "s", "-3")],
    [("ZADD", "z", "1", "a"), ("ZRANDMEMBER", "z", "-3", "WITHSCORES")],
    # --- SCAN MATCH/COUNT/TYPE ---
    [("MSET", "k1", "1", "k2", "2", "k3", "3"), ("SCAN", "0", "MATCH", "k*", "COUNT", "100")],
    [("RPUSH", "lst", "1"), ("SET", "str", "v"), ("SCAN", "0", "TYPE", "string", "COUNT", "100")],
    # --- LINSERT / LSET / LREM ---
    [("RPUSH", "L", "a", "b", "c"), ("LINSERT", "L", "BEFORE", "b", "X"), ("LRANGE", "L", "0", "-1")],
    [("RPUSH", "L", "a", "b", "c"), ("LINSERT", "L", "AFTER", "z", "X")],
    [("RPUSH", "L", "a", "b", "a", "c", "a"), ("LREM", "L", "-2", "a"), ("LRANGE", "L", "0", "-1")],
    [("RPUSH", "L", "a", "b", "c"), ("LSET", "L", "1", "X"), ("LRANGE", "L", "0", "-1")],
    [("RPUSH", "L", "a"), ("LSET", "L", "5", "X")],
]


def run_scenario(c, steps):
    c.cmd("FLUSHALL")
    replies = []
    for step in steps:
        r = c.cmd(*step)
        # SCAN returns [cursor, [keys...]] in dict-bucket order (fr uses BTreeSet
        # ordering) — the key SET is what matters, not the traversal order. Sort
        # the key list so this stays a meaningful gate without the WONTFIX noise.
        if step[0] == "SCAN" and isinstance(r, list) and len(r) == 2 and isinstance(r[1], list):
            r = [r[0], sorted(r[1])]
        replies.append(r)
    return replies


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    o, f = Conn(args.oracle), Conn(args.fr)

    diffs = 0
    for steps in SCENARIOS:
        ro = run_scenario(o, steps)
        rf = run_scenario(f, steps)
        if ro != rf:
            # find first diverging step
            for i, (a, b) in enumerate(zip(ro, rf)):
                if a != b:
                    diffs += 1
                    print(f"DIFF in {steps}")
                    print(f"   step {i}: {steps[i]}")
                    print(f"   oracle: {a!r}")
                    print(f"   fr    : {b!r}")
                    break
    if diffs:
        print(f"\nFAIL: {diffs} divergences")
        sys.exit(1)
    print(f"OK: {len(SCENARIOS)} edge scenarios byte-exact vs redis 7.2.4")


if __name__ == "__main__":
    main()
