#!/usr/bin/env python3
"""Differential OBJECT IDLETIME / OBJECT FREQ across maxmemory-policy switches,
frankenredis vs vendored redis 7.2.4.

This exercises the LRU/LFU access-recency metadata that the reply/digest fuzzers
miss: OBJECT IDLETIME and OBJECT FREQ are policy-gated and depend on the packed
per-key access metadata, not on the value bytes (so DEBUG DIGEST agrees while
IDLETIME can still diverge). Covers the lfulog FREQ-init and the nro98
LFU->LRU-switch reinterpretation paths.

Byte-matchable HARD checks: policy-gated errors, fresh IDLETIME == 0, FREQ init,
LFU->LRU re-access clearing stale LFU metadata, and the nro98 reinterpretation
producing a non-zero idle on BOTH servers when there is no re-access.

Usage: lfu_idletime_policy_differ.py <oracle_port> <fr_port>
       Exit 0 = parity,
            1 = NEW divergence, 2 = setup error.
"""
import socket, sys


def C(p):
    return socket.create_connection(("127.0.0.1", p), timeout=10)


class R:
    def __init__(s, p):
        s.s = C(p)
        s.buf = b""

    def _l(s):
        while b"\r\n" not in s.buf:
            s.buf += s.s.recv(1 << 20)
        l, s.buf = s.buf.split(b"\r\n", 1)
        return l

    def _n(s, n):
        while len(s.buf) < n + 2:
            s.buf += s.s.recv(1 << 20)
        d = s.buf[:n]
        s.buf = s.buf[n + 2:]
        return d

    def read(s):
        l = s._l()
        t = l[:1]
        if t in (b"+", b":", b"-"):
            return l.decode("latin1")
        if t == b"$":
            n = int(l[1:])
            return None if n < 0 else s._n(n).decode("latin1")
        if t == b"*":
            n = int(l[1:])
            return None if n < 0 else [s.read() for _ in range(n)]
        return l.decode("latin1")

    def cmd(s, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else x
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        s.s.sendall(o)
        return s.read()


OR = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
FR = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
od = R(OR)
fr = R(FR)

fails = []


def both(*cmd):
    return od.cmd(*cmd), fr.cmd(*cmd)


def hard(label, *cmd):
    o, f = both(*cmd)
    if o != f:
        fails.append(f"{label}: cmd={list(cmd)} redis={o!r} fr={f!r}")
    return o, f


def reset(policy):
    for d in (od, fr):
        d.cmd("config", "set", "maxmemory-policy", policy)
        d.cmd("flushall")


def as_int(x):
    # RESP integer replies arrive as ":<n>"; tolerate a leading ':'/'+'.
    if isinstance(x, str):
        x = x.lstrip(":+")
    return int(x)


def small_int(x):
    try:
        return as_int(x) <= 1
    except (TypeError, ValueError):
        return False


def big_int(x):
    try:
        return as_int(x) > 1
    except (TypeError, ValueError):
        return False


# --- A: fresh IDLETIME under a non-LFU policy is 0 on both ---
reset("noeviction")
both("set", "k", "v")
hard("A_fresh_idletime_zero", "object", "idletime", "k")

# --- B: OBJECT IDLETIME under an LFU policy errors identically ---
reset("allkeys-lfu")
both("set", "k", "v")
hard("B_idletime_under_lfu_err", "object", "idletime", "k")

# --- C: OBJECT FREQ init value under LFU (LFU_INIT_VAL) ---
hard("C_freq_init", "object", "freq", "k")

# --- D: OBJECT FREQ under a non-LFU policy errors identically ---
reset("noeviction")
both("set", "k", "v")
hard("D_freq_under_nonlfu_err", "object", "freq", "k")

# --- E (frankenredis-97wc2): LFU access, switch to non-LFU, re-access, then
#     IDLETIME. redis resets robj.lru on the non-LFU read so idle is ~0; fr
#     must also clear the stale LFU reinterpretation marker on that read. ---
reset("allkeys-lfu")
both("set", "k", "v")
both("get", "k")
for d in (od, fr):
    d.cmd("config", "set", "maxmemory-policy", "noeviction")
both("get", "k")
o, f = both("object", "idletime", "k")
if not (small_int(o) and small_int(f)):
    fails.append(
        f"E_reaccess_idletime: redis={o!r} fr={f!r} "
        "(non-LFU read must clear the LFU-bits reinterpretation)"
    )

# --- F: nro98 — LFU set, switch to non-LFU, NO re-access; both reinterpret the
#     stale LFU bits into a non-zero idle (the bug nro98 fixed: fr used to be 0). ---
reset("allkeys-lfu")
both("set", "k", "v")
for d in (od, fr):
    d.cmd("config", "set", "maxmemory-policy", "noeviction")
o, f = both("object", "idletime", "k")
if not (big_int(o) and big_int(f)):
    fails.append(
        f"F_nro98_reinterpret_nonzero: redis={o!r} fr={f!r} "
        "(both should reinterpret stale LFU bits to a non-zero idle)"
    )

print("=" * 60)
if fails:
    print(f"FAIL — {len(fails)} NEW divergence(s) in OBJECT IDLETIME/FREQ vs redis 7.2.4:")
    for x in fails:
        print(f"  {x}")
    sys.exit(1)
print(
    "PASS — OBJECT IDLETIME/FREQ policy-switch behavior matches redis 7.2.4 "
    "(hard checks A-F)"
)
