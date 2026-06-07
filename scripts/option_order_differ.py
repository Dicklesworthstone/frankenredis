#!/usr/bin/env python3
"""Curated edge-case differential probe for still-unfuzzed option parsers,
targeting validation-ORDER (type-check vs arg-validation precedence) and
reply-shape bugs. fr=:18391 oracle=:18390.

For sampling commands (HRANDFIELD/ZRANDMEMBER/SRANDMEMBER) only the error class
and reply *length* are compared (element identity is randomized).
"""
import socket, sys

def conn(p):
    s = socket.create_connection(("127.0.0.1", p)); s.settimeout(3); return s

def cmd(s, *a):
    b = b"*%d\r\n" % len(a)
    for x in a:
        if isinstance(x, str): x = x.encode()
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
diffs = []

def seed(s):
    cmd(s, "FLUSHALL")
    cmd(s, "SET", "str", "hello")
    cmd(s, "RPUSH", "lst", "a", "b", "c")
    cmd(s, "SADD", "set", "x", "y", "z")
    cmd(s, "HSET", "hash", "f1", "v1", "f2", "v2")
    cmd(s, "ZADD", "zs", "1", "m1", "2", "m2", "3", "m3")

def run(label, args, shape_only=False):
    seed(R); seed(F)
    a = cmd(R, *args); b = cmd(F, *args)
    if shape_only:
        def shp(x):
            t, v = x
            if t == "*": return ("*", len(v) if v is not None else None)
            if t == "-": return ("-", v)
            if t == "$": return ("$", "bulk" if v is not None else None)
            return (t, "val")
        ok = shp(a) == shp(b)
    else:
        ok = a == b
    tag = "OK  " if ok else "DIFF"
    if not ok:
        diffs.append((label, args, a, b))
    print(f"[{tag}] {label}: {' '.join(map(str,args))}\n        O={a!r}\n        F={b!r}")

# ---- SETRANGE / GETRANGE offset edges ----
run("setrange neg offset", ["SETRANGE", "str", "-1", "x"])
run("setrange huge offset", ["SETRANGE", "str", "536870912", "x"])  # >512MB-1
run("setrange offset on wrongtype", ["SETRANGE", "lst", "-1", "x"])  # neg + wrongtype
run("setrange offset wrongtype empty val", ["SETRANGE", "lst", "0", ""])
run("getrange wrongtype non-int range", ["GETRANGE", "lst", "abc", "xyz"])
run("getrange non-int start", ["GETRANGE", "str", "x", "2"])

# ---- LMPOP / ZMPOP numkeys + COUNT ----
run("lmpop numkeys 0", ["LMPOP", "0", "LEFT"])
run("lmpop bad numkeys", ["LMPOP", "x", "lst", "LEFT"])
run("lmpop count 0", ["LMPOP", "1", "lst", "LEFT", "COUNT", "0"])
run("lmpop count neg", ["LMPOP", "1", "lst", "LEFT", "COUNT", "-1"])
run("lmpop no dir", ["LMPOP", "1", "lst"])
run("lmpop wrongtype key", ["LMPOP", "1", "str", "LEFT"])
run("lmpop wrongtype + bad count", ["LMPOP", "1", "str", "LEFT", "COUNT", "-1"])
run("zmpop numkeys 0", ["ZMPOP", "0", "MIN"])
run("zmpop count 0", ["ZMPOP", "1", "zs", "MIN", "COUNT", "0"])
run("zmpop wrongtype + bad count", ["ZMPOP", "1", "str", "MIN", "COUNT", "0"])
run("zmpop both min max", ["ZMPOP", "1", "zs", "MIN", "MAX"])

# ---- SINTERCARD / LCS / SMOVE ----
run("sintercard numkeys 0", ["SINTERCARD", "0"])
run("sintercard limit neg", ["SINTERCARD", "1", "set", "LIMIT", "-1"])
run("sintercard wrongtype + bad limit", ["SINTERCARD", "1", "str", "LIMIT", "-1"])
run("smove wrongtype src", ["SMOVE", "str", "set", "x"])
run("smove wrongtype dst", ["SMOVE", "set", "str", "x"])

# ---- LPUSH/RPUSH/LINSERT/LSET variadic wrongtype ----
run("lpush wrongtype", ["LPUSH", "str", "a", "b"])
run("linsert wrongtype", ["LINSERT", "str", "BEFORE", "a", "b"])
run("linsert bad where", ["LINSERT", "lst", "SIDEWAYS", "a", "b"])
run("lset wrongtype", ["LSET", "str", "0", "x"])
run("lset out of range", ["LSET", "lst", "99", "x"])

# ---- GETEX / GETDEL / COPY ----
run("getex wrongtype + bad opt", ["GETEX", "lst", "EX", "abc"])
run("getex ex 0", ["GETEX", "str", "EX", "0"])
run("getex persist + ex", ["GETEX", "str", "PERSIST", "EX", "10"])
run("getdel wrongtype", ["GETDEL", "lst"])
run("copy bad db", ["COPY", "str", "dst", "DB", "-1"])
run("copy same key", ["COPY", "str", "str"])

# ---- ZADD / ZRANGEBYSCORE / ZRANGESTORE flag edges ----
run("zadd gt lt", ["ZADD", "zs", "GT", "LT", "1", "m1"])
run("zadd nx gt", ["ZADD", "zs", "NX", "GT", "1", "m1"])
run("zadd nx xx", ["ZADD", "zs", "NX", "XX", "1", "m1"])
run("zrangebyscore bad min", ["ZRANGEBYSCORE", "zs", "notanum", "5"])
run("zrangestore wrongtype src", ["ZRANGESTORE", "dst", "str", "0", "-1"])
run("zrange rev limit non-byscore", ["ZRANGE", "zs", "0", "-1", "LIMIT", "0", "5"])

# ---- SRANDMEMBER / ZRANDMEMBER / HRANDFIELD (shape only) ----
run("srandmember neg count", ["SRANDMEMBER", "set", "-5"], shape_only=True)
run("srandmember wrongtype", ["SRANDMEMBER", "str", "-5"])
run("zrandmember neg withscores", ["ZRANDMEMBER", "zs", "-4", "WITHSCORES"], shape_only=True)
run("zrandmember wrongtype", ["ZRANDMEMBER", "str", "2"])
run("hrandfield neg withvalues", ["HRANDFIELD", "hash", "-4", "WITHVALUES"], shape_only=True)
run("hrandfield wrongtype", ["HRANDFIELD", "str", "2"])
run("hrandfield extra arg", ["HRANDFIELD", "hash", "2", "WITHVALUES", "EXTRA"])

# ---- EXPIRE family flag combos ----
run("expire nx xx", ["EXPIRE", "str", "100", "NX", "XX"])
run("expire bad flag", ["EXPIRE", "str", "100", "ZZ"])
run("expire wrongtype-noexist + bad flag", ["EXPIRE", "nope", "100", "ZZ"])

print(f"\n==== TOTAL DIFFS: {len(diffs)} ====")
for label, args, a, b in diffs:
    print(f"  {label}: {' '.join(map(str,args))}\n     O={a!r}\n     F={b!r}")
sys.exit(1 if diffs else 0)
