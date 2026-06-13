#!/usr/bin/env python3
"""Golden proof for frankenredis-geowrongtype: GEORADIUS/GEOSEARCH/GEOSEARCHSTORE
type-check the (source) key before parsing shape/unit/radius/count options, so a
wrong-type key surfaces WRONGTYPE ahead of any option error — matching upstream
geo.c (lookupKeyRead + checkType run first). Covers wrong-type x garbage-option
combos, missing-key fall-through (options still parsed), and valid isomorphism.
fr=:18391 oracle=:18390. Replies sha256'd; passes iff digests match.
"""
import socket, sys, hashlib

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
    if t in (b"+", b"-", b":"): return l
    if t == b"$":
        n = int(r)
        if n == -1: return b"$-1"
        d = b""
        while len(d) < n + 2: d += s.recv(n + 2 - len(d))
        return b"$" + d[:n]
    if t == b"*":
        n = int(r)
        if n == -1: return b"*-1"
        return b"*[" + b",".join(rd(s) for _ in range(n)) + b"]"
    return l

WRONG = ["str", "lst", "set", "hash"]
CASES = []
# wrong-type source x garbage options -> WRONGTYPE for all
for wt in WRONG:
    CASES += [
        ["GEORADIUS", wt, "15", "37", "m", "WITHHASH"],
        ["GEORADIUS", wt, "15", "37", "-1", "X", "COUNT", "-1"],
        ["GEORADIUS", wt, "abc", "37", "100", "km"],
        ["GEOSEARCH", wt, "FROMLONLAT", "15", "37", "BYBOX", "-1", "-1", "X"],
        ["GEOSEARCH", wt, "FROMMEMBER", "Palermo", "BYRADIUS", "abc", "km"],
        ["GEOSEARCH", wt, "FROMLONLAT", "15", "37", "BYRADIUS", "999999999999", "~", "COUNT", "-100"],
        ["GEOSEARCHSTORE", "dst", wt, "FROMLONLAT", "15", "37", "BYRADIUS", "-1", "X"],
        ["GEOSEARCHSTORE", "dst", wt, "FROMMEMBER", "x", "BYBOX", "-1", "-1", "m"],
    ]
# missing key: options still parsed (both servers) -> option error or empty
CASES += [
    ["GEORADIUS", "none", "15", "37", "m", "WITHHASH"],          # -> need numeric radius
    ["GEORADIUS", "none", "15", "37", "100", "km"],              # -> empty
    ["GEOSEARCH", "none", "FROMLONLAT", "15", "37", "BYBOX", "-1", "-1", "X"],  # -> error
    ["GEOSEARCH", "none", "FROMLONLAT", "15", "37", "BYRADIUS", "100", "km"],   # -> empty
    ["GEOSEARCHSTORE", "dst", "none", "FROMLONLAT", "15", "37", "BYRADIUS", "100", "km"],
]
# valid zset isomorphism
CASES += [
    ["GEORADIUS", "geo", "15", "37", "200", "km", "ASC"],
    ["GEORADIUS", "geo", "15", "37", "200", "km", "WITHDIST", "WITHCOORD", "COUNT", "1"],
    ["GEOSEARCH", "geo", "FROMLONLAT", "15", "37", "BYRADIUS", "200", "km", "ASC", "WITHCOORD"],
    ["GEOSEARCH", "geo", "FROMMEMBER", "Palermo", "BYBOX", "400", "400", "km", "DESC"],
    ["GEOSEARCHSTORE", "dst", "geo", "FROMLONLAT", "15", "37", "BYRADIUS", "200", "km"],
    ["GEODIST", "geo", "Palermo", "Catania", "km"],
]

def transcript(port):
    s = conn(port)
    out = []
    for c in CASES:
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "str", "v")
        cmd(s, "RPUSH", "lst", "a")
        cmd(s, "SADD", "set", "m")
        cmd(s, "HSET", "hash", "f", "v")
        cmd(s, "GEOADD", "geo", "13.361389", "38.115556", "Palermo",
            "15.087269", "37.502669", "Catania")
        out.append(b" ".join(x.encode() for x in c) + b" => " + cmd(s, *c))
    return b"\n".join(out)

_pp = [int(x) for x in sys.argv[1:] if x.isdigit()]
_op = _pp[0] if len(_pp) > 0 else 18390
_fp = _pp[1] if len(_pp) > 1 else 18391
a = transcript(_op); b = transcript(_fp)
ha = hashlib.sha256(a).hexdigest(); hb = hashlib.sha256(b).hexdigest()
if "--emit" in sys.argv:
    print(f"oracle sha256 = {ha}  ({len(a)} bytes)")
    print(f"fr     sha256 = {hb}  ({len(b)} bytes)")
match = ha == hb
print(f"GOLDEN MATCH: {match}")
if not match:
    for i, (x, y) in enumerate(zip(a.split(b"\n"), b.split(b"\n"))):
        if x != y:
            print(f"  first diff @ line {i}:\n    O={x!r}\n    F={y!r}")
            break
sys.exit(0 if match else 1)
