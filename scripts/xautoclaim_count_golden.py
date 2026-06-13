#!/usr/bin/env python3
"""Golden proof for frankenredis-zlpqd: XAUTOCLAIM bounds COUNT to
[1, i64::MAX/16] (upstream getRangeLongFromObjectOrReply with
max_count = LONG_MAX / max(sizeof(streamID)=16, attempts_factor=10)), and the
range-check runs BEFORE the key type-check — so a too-large count surfaces
'COUNT must be > 0' even on a wrong-type key. fr=:18391 oracle=:18390. Replies
sha256'd; passes iff digests match.
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

MAX = "576460752303423487"     # i64::MAX / 16
OVER = "576460752303423488"    # MAX + 1
COUNTS = ["0", "-1", "abc", "1", "100", MAX, OVER, "9223372036854775807"]
KEYS = ["wt", "st", "none"]    # wt=wrong-type, st=valid stream (no group), none=missing

CASES = []
for key in KEYS:
    for cnt in COUNTS:
        CASES.append(["XAUTOCLAIM", key, "g", "c", "0", "0", "COUNT", cnt])
        CASES.append(["XAUTOCLAIM", key, "g", "c", "0", "0", "COUNT", cnt, "JUSTID"])
# argument-precedence: bad minidle / bad start beat COUNT-range and type-check
CASES += [
    ["XAUTOCLAIM", "wt", "g", "c", "abc", "0", "COUNT", OVER],
    ["XAUTOCLAIM", "wt", "g", "c", "0", "badid", "COUNT", OVER],
    ["XAUTOCLAIM", "wt", "g", "c", "0", "0", "BADOPT"],
    ["XAUTOCLAIM", "st", "g", "c", "0", "0"],     # valid stream, no group -> NOGROUP
]

def transcript(port):
    s = conn(port)
    out = []
    for c in CASES:
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "wt", "v")
        cmd(s, "XADD", "st", "1-1", "f", "v")
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
