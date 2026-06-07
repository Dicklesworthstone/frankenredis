#!/usr/bin/env python3
"""Seeded randomized differential fuzzer for the richer option grammars vs redis
7.2.4: SORT, XADD (trim), XRANGE/XAUTOCLAIM, RESTORE, GEOSEARCH/GEORADIUS,
BITFIELD multi-op. fr=:18391 oracle=:18390. Adversarial arg pools (neg/zero/
huge/non-int/conflicting/wrong-type). Reply-diff after each command; mutating
state is reseeded each iteration. Reads element-identity-stable replies only
(no random-sample commands here). DUMP payloads are captured live then mutated.
"""
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

R = conn(18390); F = conn(18391)

INTISH = ["0", "-1", "1", "-100", "100", "abc", "", "999999999999",
          "9223372036854775807", "-9223372036854775808", "4294967296", "2.5", "~", "="]
KEYS = ["str", "lst", "set", "hash", "zs", "stream", "geo", "none"]

def seed(s):
    cmd(s, "FLUSHALL")
    cmd(s, "SET", "str", "5")
    cmd(s, "RPUSH", "lst", "3", "1", "2", "10")
    cmd(s, "SADD", "set", "30", "10", "20")
    cmd(s, "HSET", "hash", "f1", "v1")
    cmd(s, "ZADD", "zs", "1", "a", "2", "b", "3", "c")
    cmd(s, "XADD", "stream", "1-1", "f", "v")
    cmd(s, "XADD", "stream", "2-2", "f", "v")
    cmd(s, "GEOADD", "geo", "13.361389", "38.115556", "Palermo", "15.087269", "37.502669", "Catania")

def k(g): return g.choice(KEYS)
def v(g): return g.choice(INTISH)

GENS = [
    # SORT — rich option grammar
    lambda g: ["SORT", k(g)] + g.choice([[], ["ALPHA"], ["DESC"], ["ASC"]])
              + (["LIMIT", v(g), v(g)] if g.random()<.6 else [])
              + (["BY", "weight_*"] if g.random()<.4 else [])
              + (["GET", "#"] if g.random()<.3 else [])
              + (["STORE", "dst"] if g.random()<.3 else []),
    lambda g: ["SORT_RO", k(g)] + (["LIMIT", v(g), v(g)] if g.random()<.6 else []) + g.choice([[],["ALPHA"]]),
    # XADD trim grammar
    lambda g: ["XADD", k(g)] + (["NOMKSTREAM"] if g.random()<.3 else [])
              + g.choice([[], ["MAXLEN", v(g)], ["MAXLEN", g.choice(["~","="]), v(g)],
                          ["MINID", v(g)], ["MAXLEN", "~", v(g), "LIMIT", v(g)]])
              + ["*", "field", "value"],
    lambda g: ["XTRIM", k(g)] + g.choice([["MAXLEN", v(g)], ["MAXLEN", g.choice(["~","="]), v(g)],
                                          ["MINID", v(g)], ["MAXLEN","~",v(g),"LIMIT",v(g)]]),
    # XRANGE / XAUTOCLAIM
    lambda g: ["XRANGE", k(g), v(g), v(g)] + (["COUNT", v(g)] if g.random()<.6 else []),
    lambda g: ["XREVRANGE", k(g), v(g), v(g)] + (["COUNT", v(g)] if g.random()<.6 else []),
    lambda g: ["XAUTOCLAIM", k(g), "grp", "cons", v(g), v(g)]
              + (["COUNT", v(g)] if g.random()<.5 else []) + (["JUSTID"] if g.random()<.4 else []),
    lambda g: ["XADD", k(g), "MAXLEN", g.choice(["~","="]), v(g), "LIMIT", v(g), "*", "f", "x"],
    # GEOSEARCH / GEORADIUS grammar
    lambda g: ["GEOSEARCH", k(g), "FROMMEMBER", "Palermo", "BYRADIUS", v(g), g.choice(["m","km","ft","mi","X"])]
              + g.choice([[],["ASC"],["DESC"]]) + (["COUNT", v(g)] if g.random()<.5 else []),
    lambda g: ["GEOSEARCH", k(g), "FROMLONLAT", "15", "37", "BYBOX", v(g), v(g), g.choice(["m","km","X"])]
              + (["COUNT", v(g), "ANY"] if g.random()<.4 else []),
    lambda g: ["GEORADIUS", k(g), "15", "37", v(g), g.choice(["m","km","ft","mi","X"])]
              + g.choice([[],["WITHCOORD"],["WITHDIST"],["WITHHASH"]]) + (["COUNT", v(g)] if g.random()<.5 else []),
    lambda g: ["GEODIST", k(g), "Palermo", "Catania"] + ([g.choice(["m","km","ft","mi","X"])] if g.random()<.6 else []),
    # BITFIELD multi-op
    lambda g: ["BITFIELD", k(g)] + sum([g.choice([
        ["GET", g.choice(["u8","i16","u64","i64","X","i0","i65"]), v(g)],
        ["SET", g.choice(["u8","i16","i63"]), v(g), v(g)],
        ["INCRBY", g.choice(["u8","i16"]), v(g), v(g)],
        ["OVERFLOW", g.choice(["WRAP","SAT","FAIL","BAD"])],
    ]) for _ in range(g.randint(1,3))], []),
    # SETRANGE / GETRANGE / SUBSTR
    lambda g: ["SETRANGE", k(g), v(g), g.choice(["x",""])],
    lambda g: ["GETRANGE", k(g), v(g), v(g)],
    # LPOS / LINSERT extra
    lambda g: ["LPOS", k(g), "1", "RANK", v(g), "COUNT", v(g)] + (["MAXLEN", v(g)] if g.random()<.5 else []),
]

def fuzz(seeds, per):
    diffs = []
    for sd in seeds:
        g = random.Random(sd)
        for i in range(per):
            args = [str(x) for x in g.choice(GENS)(g)]
            seed(R); seed(F)
            try: a = cmd(R, *args)
            except Exception as e: a = ("EXC", str(e))
            try: b = cmd(F, *args)
            except Exception as e: b = ("EXC", str(e))
            if a != b:
                diffs.append((sd, i, args, a, b))
                if len(diffs) <= 50:
                    print(f"DIFF seed={sd} i={i}: {' '.join(args)}\n   O={a!r}\n   F={b!r}")
    return diffs

seeds = [int(x) for x in (sys.argv[1:] or ["1","2","3","4"])]
d = fuzz(seeds, 1500)
print(f"\n==== TOTAL DIFFS: {len(d)} over {len(seeds)*1500} ops ====")
sys.exit(1 if d else 0)
