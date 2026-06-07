#!/usr/bin/env python3
"""Golden proof for frankenredis-4zv7a: LSET truncates its index to a 32-bit int
like upstream listTypeReplaceAtIndex(int index). fr=:18391 oracle=:18390.
Covers the truncation boundary (i64::MAX -> -1 -> last, i64::MIN/2^32 -> 0, just
out of range) plus normal in-range LSET isomorphism, on both a small (listpack)
and a larger list. Replies + resulting list state are sha256'd; passes iff equal.
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

I64MAX = "9223372036854775807"
I64MIN = "-9223372036854775808"
INDICES = [I64MAX, I64MIN, "4294967296", "4611686018427387904",
           "9223372036854775804", "9223372036854775803", "9223372036854775805",
           "-9223372036854775805", "-9223372036854775804", "2147483648",
           "2147483647", "0", "-1", "3", "-4", "4", "-5", "100"]

def transcript(port, n):
    s = conn(port)
    elems = [f"e{i}" for i in range(n)]
    out = []
    for idx in INDICES:
        cmd(s, "DEL", "lst")
        cmd(s, "RPUSH", "lst", *elems)
        r = cmd(s, "LSET", "lst", idx, "X")
        st = cmd(s, "LRANGE", "lst", "0", "-1")
        out.append(f"n={n} LSET {idx}".encode() + b" => " + r + b" | " + st)
    return b"\n".join(out)

def full(port):
    return transcript(port, 4) + b"\n" + transcript(port, 200)

a = full(18390); b = full(18391)
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
