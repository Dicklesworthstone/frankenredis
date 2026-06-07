#!/usr/bin/env python3
"""Golden proof for frankenredis-8g0ad: ZUNIONSTORE/ZINTERSTORE/ZDIFFSTORE/
ZUNION/ZINTER/ZDIFF type-check the source keys (ZSET/SET ok, else WRONGTYPE)
BEFORE parsing the WEIGHTS/AGGREGATE/WITHSCORES options, matching upstream
zunionInterDiffGenericCommand (t_zset.c:2603-2621 key checkType precedes the
option loop at :2623). The numkeys-overflow syntax check still precedes the
type-check. fr=:18391 oracle=:18390. Replies sha256'd; passes iff digests match.
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

CASES = [
    # wrong-type source + malformed option -> WRONGTYPE (not syntax error)
    ["ZINTERSTORE", "dst", "1", "str", "str", "str"],
    ["ZINTERSTORE", "dst", "3", "str", "none", "WEIGHTS", "0", "0"],
    ["ZUNIONSTORE", "dst", "1", "str", "WEIGHTS"],
    ["ZUNIONSTORE", "dst", "1", "str", "str", "WEIGHTS", "AGGREGATE", "MAX"],
    ["ZUNIONSTORE", "dst", "2", "str", "zs", "WEIGHTS", "1", "3", "AGGREGATE", "BAD"],
    ["ZINTER", "1", "str", "AGGREGATE", "BAD"],
    ["ZUNION", "1", "str", "set", "AGGREGATE", "MAX"],
    ["ZUNION", "2", "none", "str", "WEIGHTS", "0", "2"],
    ["ZDIFF", "2", "str", "none", "WEIGHTS", "-1", "WITHSCORES"],
    ["ZDIFF", "3", "zs", "str", "set", "WEIGHTS", "AGGREGATE", "BAD", "WITHSCORES"],
    ["ZDIFFSTORE", "dst", "2", "set", "str", "WEIGHTS"],
    ["ZDIFFSTORE", "dst", "1", "str", "str", "zs", "AGGREGATE", "MIN"],
    # set source is VALID (not wrong-type) — proceeds to option syntax / result
    ["ZUNIONSTORE", "dst", "1", "set", "AGGREGATE", "BAD"],   # syntax error (set ok, bad agg)
    ["ZINTERSTORE", "dst", "2", "set", "zs"],                  # ok -> count
    # numkeys overflow still beats the type-check (syntax error)
    ["ZUNIONSTORE", "dst", "9", "str"],
    ["ZINTER", "9", "str"],
    # missing-key source is fine
    ["ZUNIONSTORE", "dst", "2", "none", "none", "AGGREGATE", "BAD"],  # syntax (missing ok, bad agg)
    ["ZDIFF", "2", "none", "zs"],
    # valid isomorphism
    ["ZUNIONSTORE", "dst", "2", "zs", "zs2", "WEIGHTS", "2", "3", "AGGREGATE", "MAX"],
    ["ZINTERSTORE", "dst", "2", "zs", "zs2", "WEIGHTS", "1", "1"],
    ["ZUNION", "2", "zs", "zs2", "WITHSCORES"],
    ["ZINTER", "2", "zs", "set", "AGGREGATE", "MIN", "WITHSCORES"],
    ["ZDIFF", "2", "zs", "zs2", "WITHSCORES"],
    ["ZDIFFSTORE", "dst", "2", "zs", "zs2"],
]

def transcript(port):
    s = conn(port)
    out = []
    for c in CASES:
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "str", "v")
        cmd(s, "SADD", "set", "a", "x")
        cmd(s, "ZADD", "zs", "1", "a", "2", "b", "3", "c")
        cmd(s, "ZADD", "zs2", "5", "a", "6", "d")
        out.append(b" ".join(x.encode() for x in c) + b" => " + cmd(s, *c))
        if c[0].endswith("STORE"):
            out.append(b"  dst => " + cmd(s, "ZRANGE", "dst", "0", "-1", "WITHSCORES"))
    return b"\n".join(out)

a = transcript(18390); b = transcript(18391)
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
