#!/usr/bin/env python3
"""Golden proof for frankenredis-qdla6: XGROUP SETID parses ENTRIESREAD before
the key/group/id existence checks, and accepts the non-strict `-`/`+` ids.
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

CASES = [
    # ENTRIESREAD value error precedes every existence/id error
    ["XGROUP", "SETID", "none", "grp", "1-1", "ENTRIESREAD", "abc"],   # vs key-required
    ["XGROUP", "SETID", "str", "nope", "abc", "ENTRIESREAD", "abc"],   # vs WRONGTYPE / id
    ["XGROUP", "SETID", "st", "nope", "0", "ENTRIESREAD", "abc"],      # vs NOGROUP
    ["XGROUP", "SETID", "st", "grp", "+", "ENTRIESREAD", "abc"],       # vs valid id
    ["XGROUP", "SETID", "st", "grp", "0", "ENTRIESREAD", "-9"],        # positive-or-1 error
    ["XGROUP", "SETID", "st", "grp", "0", "BADOPT", "5"],              # bad trailing token
    # +/- ids accepted (non-strict)
    ["XGROUP", "SETID", "st", "grp", "+"],
    ["XGROUP", "SETID", "st", "grp", "-"],
    ["XGROUP", "SETID", "st", "grp", "$"],
    ["XGROUP", "SETID", "st", "grp", "5-5"],
    ["XGROUP", "SETID", "st", "grp", "(5"],                            # interval rejected
    ["XGROUP", "SETID", "st", "grp", "+", "ENTRIESREAD", "10"],        # valid id + valid ER
    ["XGROUP", "SETID", "st", "grp", "-", "ENTRIESREAD", "-1"],
    # existence ordering when ENTRIESREAD is valid (or absent)
    ["XGROUP", "SETID", "none", "grp", "1-1"],                         # key-required
    ["XGROUP", "SETID", "str", "grp", "1-1"],                          # WRONGTYPE
    ["XGROUP", "SETID", "st", "nope", "1-1"],                          # NOGROUP
    ["XGROUP", "SETID", "none", "grp", "1-1", "ENTRIESREAD", "5"],     # key-required (ER ok)
    # CREATE still rejects +/- (strict) — regression guard
    ["XGROUP", "CREATE", "st", "g2", "+"],
    ["XGROUP", "CREATE", "st", "g3", "-"],
    ["XGROUP", "CREATE", "none", "g4", "1-1", "ENTRIESREAD", "abc"],   # CREATE ER-before-existence
]

def transcript(port):
    s = conn(port)
    out = []
    for c in CASES:
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "str", "v")
        cmd(s, "XADD", "st", "1-1", "f", "v")
        cmd(s, "XADD", "st", "2-2", "f", "v")
        cmd(s, "XGROUP", "CREATE", "st", "grp", "0")
        out.append(b" ".join(x.encode() for x in c) + b" => " + cmd(s, *c))
        # show resulting group last-delivered-id for SETID successes
        if c[1] == "SETID":
            gi = cmd(s, "XINFO", "GROUPS", "st")
            out.append(b"  groups => " + (gi if isinstance(gi, bytes) else b"x"))
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
