#!/usr/bin/env python3
"""Seeded randomized option-parser differential fuzzer vs redis 7.2.4.
fr=:18391 oracle=:18390. Fires random commands with adversarial argument pools
(neg/zero/huge/non-int/conflicting flags/wrong-type keys) over a shared small
typed key pool, comparing the full reply after every command. Mutating commands
reset the key pool each iteration to keep the two servers in lock-step.

Excludes the documented false-positive classes: sampling (random element
identity), tiny-TTL timing races, dict/hash iteration order.
"""
import argparse
import socket, sys, random

def conn(p):
    s = socket.create_connection(("127.0.0.1", p)); s.settimeout(3); return s

def cmd(s, *a):
    b = b"*%d\r\n" % len(a)
    for x in a:
        if isinstance(x, str): x = x.encode()
        elif isinstance(x, int): x = str(x).encode()
        b += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(b); return rd(s)

def rd(s):
    l = b""
    while not l.endswith(b"\r\n"): l += s.recv(1)
    l = l[:-2]; t, r = l[:1], l[1:]
    if t in (b"+", b"-"): return (t.decode(), r.decode())
    if t == b":": return (":", int(r))
    if t == b"$":
        n = int(r)
        if n == -1: return ("$", None)
        d = b""
        while len(d) < n + 2: d += s.recv(n + 2 - len(d))
        return ("$", d[:n])
    if t == b"*":
        n = int(r)
        if n == -1: return ("*", None)
        return ("*", [rd(s) for _ in range(n)])
    return ("?", l)

_ap = argparse.ArgumentParser()
_ap.add_argument("--oracle", type=int, default=18390)
_ap.add_argument("--fr", type=int, default=18391)
_ap.add_argument("seeds", nargs="*", help="RNG seeds (default 1..6)")
_args = _ap.parse_args()
R = conn(_args.oracle); F = conn(_args.fr)

KEYS = ["str", "lst", "set", "hash", "zs", "none"]
INTISH = ["0", "-1", "1", "-100", "100", "abc", "", "9999999999999", "536870912",
          "-9223372036854775808", "9223372036854775807", "3.5", "+inf", "nan"]
FLAGS_EXP = ["NX", "XX", "GT", "LT", "EX", "PX", "EXAT", "PXAT", "PERSIST", "KEEPTTL"]
WHERE = ["LEFT", "RIGHT", "BEFORE", "AFTER", "MIN", "MAX", "SIDE", ""]

def seed(s):
    cmd(s, "FLUSHALL")
    cmd(s, "SET", "str", "hello")
    cmd(s, "RPUSH", "lst", "a", "b", "c", "d")
    cmd(s, "SADD", "set", "x", "y", "z")
    cmd(s, "HSET", "hash", "f1", "v1", "f2", "v2")
    cmd(s, "ZADD", "zs", "1", "m1", "2", "m2", "3", "m3")

def ik(rng): return rng.choice(KEYS)
def iv(rng): return rng.choice(INTISH)

GENS = [
    lambda g: ["SETRANGE", ik(g), iv(g), g.choice(["x", ""])],
    lambda g: ["GETRANGE", ik(g), iv(g), iv(g)],
    lambda g: ["LMPOP", iv(g), ik(g), g.choice(WHERE)] + (["COUNT", iv(g)] if g.random()<.5 else []),
    lambda g: ["ZMPOP", iv(g), ik(g), g.choice(WHERE)] + (["COUNT", iv(g)] if g.random()<.5 else []),
    lambda g: ["LMPOP", iv(g), ik(g), ik(g), g.choice(WHERE)] + (["COUNT", iv(g)] if g.random()<.5 else []),
    lambda g: ["SINTERCARD", iv(g), ik(g)] + (["LIMIT", iv(g)] if g.random()<.5 else []),
    lambda g: ["ZADD", ik(g)] + g.sample(["NX","XX","GT","LT","CH","INCR"], g.randint(0,3)) + [iv(g), "m1"],
    lambda g: ["GETEX", ik(g)] + ([g.choice(FLAGS_EXP), iv(g)] if g.random()<.7 else [g.choice(["PERSIST"])]),
    lambda g: ["EXPIRE", ik(g), iv(g)] + g.sample(["NX","XX","GT","LT"], g.randint(0,2)),
    lambda g: ["PEXPIRE", ik(g), iv(g)] + g.sample(["NX","XX","GT","LT"], g.randint(0,2)),
    lambda g: ["LINSERT", ik(g), g.choice(WHERE), "a", "new"],
    lambda g: ["LSET", ik(g), iv(g), "x"],
    lambda g: ["SETEX", ik(g), iv(g), "v"],
    lambda g: ["SET", ik(g), "v"] + g.sample(["NX","XX","GET","KEEPTTL"], g.randint(0,2)) + ([g.choice(["EX","PX","EXAT","PXAT"]), iv(g)] if g.random()<.5 else []),
    lambda g: ["COPY", ik(g), ik(g)] + (["DB", iv(g)] if g.random()<.4 else []) + (["REPLACE"] if g.random()<.4 else []),
    lambda g: ["ZRANGEBYSCORE", ik(g), iv(g), iv(g)] + (["LIMIT", iv(g), iv(g)] if g.random()<.5 else []),
    lambda g: ["ZRANGE", ik(g), iv(g), iv(g)] + g.sample(["REV","BYSCORE","BYLEX"], g.randint(0,2)) + (["LIMIT", iv(g), iv(g)] if g.random()<.4 else []),
    lambda g: ["ZRANGESTORE", "dst", ik(g), iv(g), iv(g)],
    lambda g: ["BITCOUNT", ik(g), iv(g), iv(g)] + ([g.choice(["BYTE","BIT","BAD"])] if g.random()<.6 else []),
    lambda g: ["BITPOS", ik(g), g.choice(["0","1","2"]), iv(g), iv(g)] + ([g.choice(["BYTE","BIT"])] if g.random()<.5 else []),
    lambda g: ["SMOVE", ik(g), ik(g), "x"],
    lambda g: ["OBJECT", "ENCODING", ik(g)],
    lambda g: ["LPOS", ik(g), "a"] + (["RANK", iv(g)] if g.random()<.5 else []) + (["COUNT", iv(g)] if g.random()<.5 else []),
    lambda g: ["INCRBY", ik(g), iv(g)],
    lambda g: ["HINCRBY", ik(g), "f1", iv(g)],
    lambda g: ["SETBIT", ik(g), iv(g), g.choice(["0","1","2"])],
    lambda g: ["GETDEL", ik(g)],
]

# commands that mutate -> reseed each run anyway (we always reseed for simplicity)
def fuzz(seeds, per):
    diffs = []
    for sd in seeds:
        g = random.Random(sd)
        for i in range(per):
            args = g.choice(GENS)(g)
            args = [str(a) for a in args]
            seed(R); seed(F)
            try:
                a = cmd(R, *args)
            except Exception as e:
                a = ("EXC", str(e))
            try:
                b = cmd(F, *args)
            except Exception as e:
                b = ("EXC", str(e))
            if a != b:
                diffs.append((sd, i, args, a, b))
                if len(diffs) <= 40:
                    print(f"DIFF seed={sd} i={i}: {' '.join(args)}\n   O={a!r}\n   F={b!r}")
    return diffs

seeds = [int(x) for x in (_args.seeds or ["1","2","3","4","5","6"])]
d = fuzz(seeds, 1500)
for s in (R, F):
    try:
        cmd(s, "FLUSHALL")
    except Exception:
        pass
print(f"\n==== TOTAL DIFFS: {len(d)} over {len(seeds)*1500} ops ====")
sys.exit(1 if d else 0)
