#!/usr/bin/env python3
"""Golden proof for frankenredis-xcaw5: ZRANGE/ZRANGESTORE LIMIT count sentinel.
Upstream's shape check is `opt_limit != -1`, so only an exact -1 count is the
"no limit" sentinel; any other count without BYSCORE/BYLEX errors. Covers the
validation-order precedence (LIMIT shape error beats type-check / index-parse)
plus isomorphism (valid BYSCORE/BYLEX LIMIT, sentinel -1, plain rank). fr=:18391
oracle=:18390. Replies are sha256'd on each side; passes iff digests match.
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
    if t in (b"+", b"-"): return l
    if t == b":": return l
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
    # validation-order: LIMIT shape error must beat type-check / index-parse
    ["ZRANGE", "hash", "1", "-100", "LIMIT", "1", "-9223372036854775808"],  # wrongtype key
    ["ZRANGE", "str", "+inf", "+inf", "LIMIT", "1", "-100"],                # wrongtype + non-int bounds
    ["ZRANGE", "zs", "9999999999999", "100", "LIMIT", "9999999999999", "-100"],
    ["ZRANGE", "zs", "0", "-1", "LIMIT", "0", "5"],                         # rank + positive count -> error
    ["ZRANGE", "zs", "0", "-1", "LIMIT", "0", "-2"],                        # rank + -2 -> error
    ["ZRANGE", "none", "0", "-1", "LIMIT", "0", "-100"],                    # missing key + bad count
    # sentinel -1 is allowed in rank mode (no error, normal result)
    ["ZRANGE", "zs", "0", "-1", "LIMIT", "0", "-1"],
    ["ZRANGE", "zs", "0", "-1", "LIMIT", "5", "-1"],
    # plain rank, no LIMIT
    ["ZRANGE", "zs", "0", "-1"],
    ["ZRANGE", "zs", "0", "-1", "WITHSCORES"],
    # valid BYSCORE / BYLEX LIMIT (isomorphism — must still apply)
    ["ZRANGE", "zs", "-inf", "+inf", "BYSCORE", "LIMIT", "1", "2"],
    ["ZRANGE", "zs", "-inf", "+inf", "BYSCORE", "LIMIT", "1", "-1"],        # -1 count = to end
    ["ZRANGE", "zs", "-inf", "+inf", "BYSCORE", "LIMIT", "0", "-100"],      # negative = to end
    ["ZRANGE", "zs", "(1", "+inf", "BYSCORE", "REV", "LIMIT", "0", "1"],
    ["ZRANGE", "zs", "[a", "[z", "BYLEX", "LIMIT", "0", "2"],
    ["ZRANGE", "zs", "-", "+", "BYLEX", "LIMIT", "1", "-1"],
    # ZRANGESTORE mirror
    ["ZRANGESTORE", "dst", "zs", "0", "-1", "LIMIT", "0", "5"],             # rank + pos -> error
    ["ZRANGESTORE", "dst", "zs", "0", "-1", "LIMIT", "0", "-100"],          # rank + -100 -> error
    ["ZRANGESTORE", "dst", "zs", "0", "-1", "LIMIT", "0", "-1"],            # sentinel ok (ignored)
    ["ZRANGESTORE", "dst", "str", "0", "-1", "LIMIT", "0", "-100"],         # wrongtype + bad count -> LIMIT error
    ["ZRANGESTORE", "dst", "zs", "-inf", "+inf", "BYSCORE", "LIMIT", "1", "2"],
    ["ZRANGESTORE", "dst", "zs", "-inf", "+inf", "BYSCORE", "LIMIT", "0", "-1"],
]

def transcript(port):
    s = conn(port)
    out = []
    for c in CASES:
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "str", "hello")
        cmd(s, "HSET", "hash", "f", "v")
        cmd(s, "ZADD", "zs", "1", "a", "2", "b", "3", "c", "4", "d")
        out.append(b" ".join(x.encode() for x in c) + b" => " + cmd(s, *c))
        # also capture dst for ZRANGESTORE success cases
        if c[0] == "ZRANGESTORE":
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
