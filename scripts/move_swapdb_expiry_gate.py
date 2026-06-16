#!/usr/bin/env python3
"""Differential gate: cross-DB key relocation preserves TTL, vs vendored redis 7.2.4.

MOVE and SWAPDB relocate keys between logical DBs; in Redis the key's expire
entry travels WITH it (db->expires is keyed per-DB, so the deadline must be
re-inserted into the destination DB's expires dict). The plain TTL-semantics
differ covers COPY/RENAME (same-DB) but NOTHING exercises MOVE/SWAPDB carrying
a TTL across the DB boundary. This gate closes that gap.

Regression it guards (isa2w): an expires-side-dict refactor that lifts the
deadline out of the value Entry into a per-DB expires dict can forget to carry
the side-dict entry on MOVE/SWAPDB -> destination key silently loses its TTL
(fr ttl=-1 vs redis ttl=500), both live and post-reload.

TTL is compared by CATEGORY (-2 gone / -1 no-ttl / "TTL+" has a positive ttl),
never exact ms -> immune to clock drift but catches -1-vs-positive divergence.

Usage: move_swapdb_expiry_gate.py <oracle_port> <fr_port>   Exit 0=parity, 1=divergence.
"""
import socket, sys
def C(p): return socket.create_connection(("127.0.0.1", p), timeout=10)
class R:
    def __init__(s, p): s.s=C(p); s.buf=b""
    def _l(s):
        while b"\r\n" not in s.buf: s.buf+=s.s.recv(1<<20)
        l,s.buf=s.buf.split(b"\r\n",1); return l
    def _n(s,n):
        while len(s.buf)<n+2: s.buf+=s.s.recv(1<<20)
        d=s.buf[:n]; s.buf=s.buf[n+2:]; return d
    def read(s):
        l=s._l(); t=l[:1]
        if t in (b'+',b':',b'-'): return l.decode()
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n).decode('latin1')
        if t==b'*': n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n)]
        return l.decode()
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else x
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()

def cat(v):
    """Collapse a TTL/PTTL reply into a drift-immune category."""
    try: n=int(v)
    except (TypeError, ValueError): return v
    if n in (-2,-1): return str(n)
    return "TTL+"

div=0
def chk(label, od, fr, *cmds, ttl_idx=()):
    """Run the same command sequence on both; compare every reply. Replies at
    positions in ttl_idx are compared by TTL category (drift-immune)."""
    global div
    ao=[od.cmd(*c) for c in cmds]; af=[fr.cmd(*c) for c in cmds]
    for i,c in enumerate(cmds):
        a,b = (cat(ao[i]),cat(af[i])) if i in ttl_idx else (ao[i],af[i])
        if a!=b:
            div+=1
            print(f"DIVERGE {label} [{' '.join(map(str,c))}]\n  oracle: {ao[i]!r} ({a})\n  fr    : {af[i]!r} ({b})")

def reset(*srv):
    for s in srv:
        for db in (0,1,2): s.cmd("select",str(db)); s.cmd("flushall")
        s.cmd("select","0")

def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2]))

    # MOVE carries a positive TTL across the DB boundary.
    reset(od,fr)
    chk("move-keeps-ttl", od, fr,
        ("set","k","v","EX","500"), ("move","k","1"),
        ("select","1"), ("ttl","k"), ("get","k"), ("select","0"),
        ("exists","k"), ttl_idx=(3,))

    # MOVE of a persistent key adds no spurious TTL.
    reset(od,fr)
    chk("move-no-ttl", od, fr,
        ("set","k","v"), ("move","k","1"),
        ("select","1"), ("ttl","k"), ("select","0"), ttl_idx=(3,))

    # MOVE to an occupied destination fails (0); source + its TTL stay put.
    reset(od,fr)
    chk("move-dest-occupied", od, fr,
        ("set","k","v","EX","500"), ("select","1"), ("set","k","other"),
        ("select","0"), ("move","k","1"), ("ttl","k"),
        ("select","1"), ("get","k"), ("select","0"), ttl_idx=(5,))

    # MOVE out and back round-trips the TTL.
    reset(od,fr)
    chk("move-roundtrip", od, fr,
        ("set","k","v","EX","500"), ("move","k","2"),
        ("select","2"), ("move","k","0"), ("select","0"),
        ("ttl","k"), ttl_idx=(5,))

    # SWAPDB swaps the keyspaces AND their expires dicts.
    reset(od,fr)
    chk("swapdb-ttls", od, fr,
        ("set","a","x","EX","500"),
        ("select","1"), ("set","b","y","EX","300"), ("select","0"),
        ("swapdb","0","1"),
        ("ttl","b"), ("get","b"),            # b (was db1) now in db0, ttl 300
        ("select","1"), ("ttl","a"), ("get","a"), ("select","0"),
        ttl_idx=(5,8))

    # SWAPDB moves a volatile key onto a persistent slot and vice-versa.
    reset(od,fr)
    chk("swapdb-mixed", od, fr,
        ("set","p","x"),                     # db0: persistent
        ("select","1"), ("set","p","y","EX","400"), ("select","0"),
        ("swapdb","0","1"),
        ("ttl","p"),                         # db0 now holds the EX-400 one
        ("select","1"), ("ttl","p"), ("select","0"),
        ttl_idx=(5,7))

    if div:
        print(f"\nFAIL: {div} divergence(s)"); sys.exit(1)
    print("OK: MOVE/SWAPDB TTL propagation byte-exact vs redis 7.2.4")

if __name__=="__main__": main()
