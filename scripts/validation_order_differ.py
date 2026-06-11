#!/usr/bin/env python3
"""validation_order_differ.py — error-condition ORDER parity vs redis 7.2.4.

When a command's arguments trip MORE THAN ONE error condition at once — e.g. a
WRONGTYPE key AND a non-integer count, or two mutually-exclusive options AND a
bad value — redis reports whichever condition its handler checks FIRST. fr must
check them in the same order or it returns a different error for the same input.
This "type-check / validation ORDER" class has produced real bugs before (the
GETRANGE wrongtype-vs-empty fix), and the single-error replay differs don't probe
it because they never construct an input that trips two conditions simultaneously.

Two halves:
  * CURATED — hand-built double-error inputs across the string/list/set/zset/hash/
    bit/expire/option-conflict surfaces.
  * RANDOM  — a seeded fuzzer that picks a random command, points it at a key of a
    random (usually WRONG) type, and feeds a mix of valid / invalid-numeric /
    conflicting-option arguments.
Both compare the error CODE (first whitespace-delimited token, e.g. WRONGTYPE /
ERR) and the reply SHAPE (error vs ok vs nil vs array) — exact wording within one
code legitimately varies and is checked elsewhere (arity_error_differ).

SETUP (oracle config-less => compiled defaults; fr strict mode):
    legacy_redis_code/redis/src/redis-server --port 16399 --save '' --appendonly no --daemonize yes
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    scripts/validation_order_differ.py 16399 16400 [seed] [iters]
"""
import socket
import sys
import random

ORACLE_DEFAULT = 16399
FR_DEFAULT = 16400


class Conn:
    def __init__(self, p):
        self.s = socket.create_connection(("127.0.0.1", p))
        self.s.settimeout(3)
        self.b = bytearray()

    def line(self):
        while b"\r\n" not in self.b:
            self.b.extend(self.s.recv(8192))
        i = self.b.index(b"\r\n")
        o = bytes(self.b[:i])
        del self.b[: i + 2]
        return o

    def rd(self):
        h = self.line()
        t = h[:1]
        if t in (b"+", b"-", b":", b",", b"#", b"("):
            return h
        if t == b"_":
            return None
        if t in (b"$", b"="):
            n = int(h[1:])
            if n < 0:
                return None
            while len(self.b) < n + 2:
                self.b.extend(self.s.recv(8192))
            d = bytes(self.b[:n])
            del self.b[: n + 2]
            return d
        if t in (b"*", b"~", b">"):
            n = int(h[1:])
            return None if n < 0 else [self.rd() for _ in range(n)]
        if t == b"%":
            n = int(h[1:])
            return [self.rd() for _ in range(2 * n)]
        return h

    def cmd(self, *a):
        buf = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else x
            buf += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(buf)
        return self.rd()


KEYS = {"L": ["RPUSH", "L", "a", "b", "c"], "H": ["HSET", "H", "f", "v"],
        "S": ["SADD", "S", "m"], "Z": ["ZADD", "Z", "1", "m"], "STR": ["SET", "STR", "hello"]}


def setup(c):
    c.cmd("FLUSHALL")
    for steps in KEYS.values():
        c.cmd(*steps)


CURATED = [
    ["SETRANGE", "L", "-1", "x"], ["SETRANGE", "L", "notnum", "x"], ["SETBIT", "L", "999999999999", "1"],
    ["SETBIT", "L", "5", "2"], ["GETRANGE", "L", "notnum", "5"], ["INCRBY", "L", "notnum"],
    ["INCR", "L"], ["APPEND", "L", "x"], ["GETDEL", "L", "extra"], ["STRLEN", "L", "extra"],
    ["SETEX", "L", "notnum", "v"], ["SETEX", "STR", "-5", "v"], ["GETEX", "L", "EX", "notnum"],
    ["EXPIRE", "L", "notnum"], ["EXPIRE", "nope", "100", "NX", "XX"], ["EXPIRE", "L", "100", "GT", "LT"],
    ["LPOP", "STR", "-1"], ["LPOP", "STR", "notnum"], ["LSET", "STR", "0", "x"], ["LINSERT", "STR", "BADWHERE", "p", "e"],
    ["SADD", "STR", "m"], ["SINTERCARD", "1", "STR", "LIMIT", "-1"], ["SRANDMEMBER", "STR", "notnum"],
    ["ZADD", "STR", "notscore", "m"], ["ZADD", "STR", "GT", "LT", "1", "m"], ["ZADD", "STR", "NX", "XX", "1", "m"],
    ["ZRANGEBYSCORE", "STR", "notfloat", "5"], ["ZADD", "Z", "NX", "GT", "1", "m"],
    ["HSET", "STR", "f"], ["HSET", "STR", "f", "v", "f2"], ["HRANDFIELD", "STR", "notnum"],
    ["BITCOUNT", "L", "0", "0", "BADMODE"], ["BITPOS", "L", "2"], ["BITPOS", "STR", "2"],
    ["GETRANGE", "STR", "notnum", "x"], ["SET", "STR", "v", "EX", "notnum"], ["SET", "STR", "v", "EX", "100", "PX", "100"],
    ["SET", "STR", "v", "EX", "0"], ["SET", "STR", "v", "KEEPTTL", "EX", "100"], ["GETEX", "STR", "EX", "100", "PERSIST"],
    ["LMPOP", "0", "LEFT"], ["LMPOP", "1", "L", "BADDIR"], ["ZADD", "Z", "INCR", "1", "a", "b"],
    ["COPY", "L", "L"], ["SMOVE", "STR", "S", "m"], ["LREM", "STR", "notnum", "x"],
    ["BITFIELD", "STR", "GET", "badtype", "0"], ["BITFIELD", "STR", "SET", "u8", "0", "notnum"],
    ["SETRANGE", "STR", "536870912", "x"], ["SETBIT", "STR", "4294967296", "1"],
]

# Random-fuzz building blocks: command templates with arg "slots" that draw from
# valid / invalid-numeric / option-conflict pools, pointed at random keys.
KEYNAMES = list(KEYS) + ["nope"]
BADNUM = ["notnum", "", "1.5x", "9999999999999999999999", "-1", "0x10", "  5", "+ "]
OKNUM = ["0", "1", "5", "100", "-1", "10"]
RANDCMDS = [
    lambda r: ["SETRANGE", r.choice(KEYNAMES), r.choice(BADNUM + OKNUM), "x"],
    lambda r: ["SETBIT", r.choice(KEYNAMES), r.choice(BADNUM + OKNUM + ["4294967296"]), r.choice(["0", "1", "2", "x"])],
    lambda r: ["GETRANGE", r.choice(KEYNAMES), r.choice(BADNUM + OKNUM), r.choice(BADNUM + OKNUM)],
    lambda r: ["INCRBY", r.choice(KEYNAMES), r.choice(BADNUM + OKNUM)],
    lambda r: ["EXPIRE", r.choice(KEYNAMES), r.choice(BADNUM + OKNUM)] + r.sample(["NX", "XX", "GT", "LT"], r.randint(0, 3)),
    lambda r: ["LPOP", r.choice(KEYNAMES), r.choice(BADNUM + OKNUM)],
    lambda r: ["LINSERT", r.choice(KEYNAMES), r.choice(["BEFORE", "AFTER", "BAD"]), "p", "e"],
    lambda r: ["SETEX", r.choice(KEYNAMES), r.choice(BADNUM + OKNUM), "v"],
    lambda r: ["GETEX", r.choice(KEYNAMES)] + r.choice([["EX", r.choice(BADNUM + OKNUM)], ["PERSIST", "EX", "1"], ["EX", "1", "PX", "1"]]),
    lambda r: ["ZADD", r.choice(KEYNAMES)] + r.sample(["NX", "XX", "GT", "LT", "CH", "INCR"], r.randint(0, 3)) + [r.choice(BADNUM + OKNUM + ["1.5", "inf"]), "m"],
    lambda r: ["SET", r.choice(KEYNAMES), "v"] + r.sample(["EX", "PX", "NX", "XX", "KEEPTTL", "GET"], r.randint(0, 3)) + ([r.choice(BADNUM + OKNUM)] if r.random() < 0.5 else []),
    lambda r: ["BITCOUNT", r.choice(KEYNAMES), r.choice(BADNUM + OKNUM), r.choice(BADNUM + OKNUM)] + r.choice([[], ["BIT"], ["BYTE"], ["BAD"]]),
    lambda r: ["BITFIELD", r.choice(KEYNAMES), r.choice(["GET", "SET", "INCRBY", "BAD"]), r.choice(["u8", "i64", "u99", "bad"]), r.choice(BADNUM + OKNUM), r.choice(BADNUM + OKNUM)],
    lambda r: ["SINTERCARD", r.choice(BADNUM + OKNUM), r.choice(KEYNAMES)] + r.choice([[], ["LIMIT", r.choice(BADNUM + OKNUM)]]),
    lambda r: ["HSET", r.choice(KEYNAMES)] + ["f", "v"][: r.randint(1, 2)] + (["x"] if r.random() < 0.3 else []),
]


def code(r):
    if isinstance(r, (bytes, bytearray)):
        if r[:1] == b"-":
            return "ERRCODE:" + r[1:].split(b" ", 1)[0].decode("latin1")
        return "STR:" + r[:1].decode("latin1")
    if isinstance(r, list):
        return "ARR"
    if r is None:
        return "NIL"
    return "OTHER"


def run_probes(o, f, probes, label, show):
    div = 0
    for p in probes:
        setup(o)
        setup(f)
        ro, rf = o.cmd(*p), f.cmd(*p)
        co, cf = code(ro), code(rf)
        if co != cf:
            div += 1
            if div <= show:
                print(f"DIVERGE [{label}] {p}\n  oracle: {ro!r}  ({co})\n  fr    : {rf!r}  ({cf})")
    return div


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else ORACLE_DEFAULT
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else FR_DEFAULT
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1234
    iters = int(sys.argv[4]) if len(sys.argv) > 4 else 4000
    o, f = Conn(op), Conn(fp)

    div = run_probes(o, f, CURATED, "curated", 30)

    rnd = random.Random(seed)
    rand_probes = [RANDCMDS[rnd.randrange(len(RANDCMDS))](rnd) for _ in range(iters)]
    div += run_probes(o, f, rand_probes, "random", 30 - min(div, 30))

    print("-" * 60)
    print(f"checked {len(CURATED)} curated + {iters} random multi-error probes; code/shape divergences: {div}")
    if div == 0:
        print("PASS — fr error-condition validation order matches redis 7.2.4")
        return 0
    print(f"FAIL — {div} divergence(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
