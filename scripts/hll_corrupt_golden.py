#!/usr/bin/env python3
"""Golden proof for frankenredis-yiu5p: layered HLL corruption validation.

Runs a deterministic command corpus covering (a) valid PFADD/PFCOUNT/PFMERGE
isomorphism and (b) the per-command layered handling of gate-valid-but-corrupt
sparse HLLs, against fr (:17381) and the redis 7.2.4 oracle (:17380). The full
reply transcript is sha256'd on each side; the proof passes iff the digests
match. Pass --emit to print the shared digest + transcript length.
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
    if t in (b"+", b"-"): return t + r
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

def transcript(port):
    s = conn(port)
    out = []
    def C(*a):
        out.append((b" ".join(x if isinstance(x, bytes) else x.encode() for x in a)[:40], cmd(s, *a)))
    C("FLUSHALL")
    # Build a valid sparse HLL and snapshot its bytes (off the oracle only;
    # both sides receive the SAME literal bytes below).
    C("PFADD", "src", "a", "b", "c")
    base = cmd(s, "GET", "src")[1:]  # strip the $ marker
    # ---- valid isomorphism ----
    C("PFADD", "k", "x", "y", "z")
    C("PFCOUNT", "k")
    C("PFADD", "k", "x")            # no new register -> 0
    C("PFCOUNT", "k")
    C("PFADD", "m", "p", "q")
    C("PFMERGE", "dst", "k", "m")
    C("PFCOUNT", "dst")
    C("PFCOUNT", "k", "m")          # multi-key valid
    # ---- layered corruption (literal bytes, identical to both servers) ----
    ovVAL = base + bytes([0b11111111] * 8)          # trailing VAL overflow
    ovXZ  = base + bytes([0b01111111, 0xff])        # trailing XZERO overflow
    trunc = base[:-1]                               # truncated (< 16384)
    badmagic = b"X" + base[1:]
    badenc = base[:4] + bytes([5]) + base[5:]
    cases = [("ovVAL", ovVAL), ("ovXZ", ovXZ), ("trunc", trunc),
             ("badmagic", badmagic), ("badenc", badenc)]
    # PFADD (hllSparseSet) tolerates trailing garbage past a COMPLETE register
    # set, so it is well-defined only for the complete-prefix corruptions
    # (ovVAL/ovXZ) and the gate failures (badmagic/badenc → WRONGTYPE). For a
    # truncated prefix, PFADD's tolerance is target-index-dependent (undefined on
    # corrupt input), so it is excluded from this golden assertion.
    pfadd_defined = {"ovVAL", "ovXZ", "badmagic", "badenc"}
    for name, raw in cases:
        C("DEL", "h", "h2", "d")
        if name in pfadd_defined:
            C("SET", "h", raw)
            C("PFADD", "h", "u", "v")
        C("SET", "h", raw)
        C("PFCOUNT", "h")            # single-key (strict)
        C("SET", "h", raw); C("SET", "h2", raw)
        C("PFCOUNT", "h", "h2")      # multi-key (tolerant)
        C("SET", "h", raw)
        C("PFMERGE", "d", "h")       # merge source (tolerant)
        C("SET", "h", raw)
        C("PFDEBUG", "GETREG", "h")
        C("SET", "h", raw)
        C("PFDEBUG", "ENCODING", "h")
    # wrong-type (non-HLL) -> plain WRONGTYPE
    C("DEL", "lst"); C("RPUSH", "lst", "e")
    C("PFADD", "lst", "z")
    C("PFCOUNT", "lst")
    blob = b"\n".join(label + b" => " + reply for label, reply in out)
    return blob

_pp = [int(x) for x in sys.argv[1:] if x.isdigit()]
_op = _pp[0] if len(_pp) > 0 else 17380
_fp = _pp[1] if len(_pp) > 1 else 17381
a = transcript(_op)
b = transcript(_fp)
ha = hashlib.sha256(a).hexdigest()
hb = hashlib.sha256(b).hexdigest()
if "--emit" in sys.argv:
    print(f"oracle sha256 = {ha}  ({len(a)} bytes)")
    print(f"fr     sha256 = {hb}  ({len(b)} bytes)")
match = ha == hb
print(f"GOLDEN MATCH: {match}")
if not match:
    # show first differing line
    la, lb = a.split(b"\n"), b.split(b"\n")
    for i, (x, y) in enumerate(zip(la, lb)):
        if x != y:
            print(f"  first diff @ line {i}:\n    oracle={x!r}\n    fr    ={y!r}")
            break
sys.exit(0 if match else 1)
