#!/usr/bin/env python3
"""Golden proof for frankenredis-vd28h: XRANGE/XREVRANGE COUNT<=0 resolves the
key before emitting the null array. Upstream parses IDs+COUNT then runs
lookupKeyReadOrReply(emptyarray)+checkType BEFORE the count==0 null reply, so a
wrong-type key -> WRONGTYPE, a missing key -> empty array, an existing stream ->
null array. fr=:18391 oracle=:18390. Replies sha256'd; passes iff digests match.
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

CASES = []
for cmdname in ("XRANGE", "XREVRANGE"):
    lo, hi = ("-", "+") if cmdname == "XRANGE" else ("+", "-")
    for key in ("str", "lst", "hash", "none", "stream"):
        for cnt in ("-1", "0", "-100", "2"):
            CASES.append([cmdname, key, lo, hi, "COUNT", cnt])
    # bad id must beat the type-check (ID parse runs first upstream)
    CASES.append([cmdname, "hash", "bad", hi, "COUNT", "-1"])
    # no-COUNT wrong-type still WRONGTYPE; no-COUNT missing -> empty
    CASES.append([cmdname, "hash", lo, hi])
    CASES.append([cmdname, "none", lo, hi])

def transcript(port):
    s = conn(port)
    out = []
    for c in CASES:
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "str", "v")
        cmd(s, "RPUSH", "lst", "a")
        cmd(s, "HSET", "hash", "f", "v")
        cmd(s, "XADD", "stream", "1-1", "f", "v")
        cmd(s, "XADD", "stream", "2-2", "g", "w")
        out.append(b" ".join(x.encode() for x in c) + b" => " + cmd(s, *c))
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
