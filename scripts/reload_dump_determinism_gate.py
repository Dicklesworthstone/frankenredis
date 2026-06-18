#!/usr/bin/env python3
"""Metamorphic gate: DUMP must be byte-stable across DEBUG RELOAD (matching redis).

Redis's DUMP of a collection is byte-deterministic across DEBUG RELOAD — RESTORE,
replication digests, and migration all depend on it. This builds a key of a given
type/encoding/size, snapshots DUMP, DEBUG RELOADs, snapshots DUMP again, and asserts
fr's stability matches redis 7.2.4's (i.e. if redis's DUMP is stable across reload,
fr's must be too). Found frankenredis-2j9wz: listpack HASH/SET re-serialize to a
different (logically-equivalent) layout after reload while redis stays stable.

Usage: reload_dump_determinism_gate.py <oracle_port> <fr_port>
Exit 0 = fr matches redis's reload-DUMP stability for every case; 1 = a regression.
"""
import socket, sys
def C(p): return socket.create_connection(("127.0.0.1",p),timeout=15)
class R:
    def __init__(s,p): s.s=C(p); s.buf=b""
    def _l(s):
        while b"\r\n" not in s.buf: s.buf+=s.s.recv(1<<20)
        l,s.buf=s.buf.split(b"\r\n",1); return l
    def _n(s,n):
        while len(s.buf)<n+2: s.buf+=s.s.recv(1<<20)
        d=s.buf[:n]; s.buf=s.buf[n+2:]; return d
    def read(s):
        l=s._l(); t=l[:1]
        if t in (b'+',b':',b'-'): return l
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n)
        if t in (b'*',b'~','%'): n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n*2 if t==b'%' else n)]
        return l
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else (str(x).encode() if not isinstance(x,bytes) else x)
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()
def build(r,kind,n):
    r.cmd("flushall")
    for c in ("hash-max-listpack-entries","zset-max-listpack-entries","set-max-listpack-entries","set-max-intset-entries"):
        r.cmd("config","set",c,"512")
    if kind=="hash":
        for i in range(n): r.cmd("hset","k","f%d"%i,"v%d"%i)
    elif kind=="list":
        for i in range(n): r.cmd("rpush","k","e%d"%i)
    elif kind=="zset":
        for i in range(n): r.cmd("zadd","k",str(i),"m%d"%i)
    elif kind=="set-lp":
        for i in range(n): r.cmd("sadd","k","m%d"%i)
    elif kind=="set-int":
        for i in range(n): r.cmd("sadd","k",str(i))
def stable(r,kind,n):
    build(r,kind,n); d1=r.cmd("dump","k"); r.cmd("debug","reload"); d2=r.cmd("dump","k")
    return d1==d2
CASES=[(k,n) for k in ("hash","list","zset","set-lp","set-int") for n in (5,50,200)]
def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2])); bad=0
    for kind,n in CASES:
        os_=stable(od,kind,n); fs=stable(fr,kind,n)
        if os_ and not fs:
            bad+=1; print(f"DIVERGE {kind}/{n}: redis DUMP stable across reload but fr's is NOT")
    print("-"*60)
    if bad: print(f"FAIL — {bad} NEW reload-DUMP-determinism regression(s) vs redis 7.2.4"); return 1
    print("PASS — fr DUMP reload-stability matches redis"); return 0
if __name__=="__main__": sys.exit(main())
