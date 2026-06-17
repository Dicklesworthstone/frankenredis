#!/usr/bin/env python3
"""Differential gate: MEMORY USAGE <key> is byte-exact across DBs, vs redis 7.2.4.

MEMORY USAGE models redis's per-key size, which counts the LOGICAL key. fr stores
keys DB-namespaced (encode_db_key), so before the b5hst fix a key in a non-zero DB
over-reported by the \\0frdb\\0+dbid prefix (db2 "s" = 80 vs redis 64). This gate
plants the same key in db 0 AND db 2 and asserts MEMORY USAGE matches redis in
BOTH — the db-0 value pins the byte-exact baseline, the db-2 value catches the
namespace inflation.

Scope: small/scalar encodings where fr's size model is byte-exact vs redis
(embstr/int/raw string, listpack list/set/hash/zset, intset). hashtable / skiplist
/ quicklist size models deterministically diverge (dict bucket array / skiplist
node overhead — architectural, kv015) and are intentionally excluded.

Usage: memory_usage_multidb_gate.py <oracle_port> <fr_port>  Exit 0=parity,1=diverge.
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
        if t in (b'+',b':',b'-'): return l.decode('latin1')
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n).decode('latin1')
        if t==b'*': n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n)]
        return l.decode('latin1')
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else x
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()

# (label, build-commands) — each builds key K of a byte-exact-modelled encoding.
KEYS = [
    ("str-embstr",  [("set","K","hello")]),
    ("str-int",     [("set","K","12345")]),
    ("str-raw",     [("set","K","x"*60)]),
    ("str-longkey", [("set","this-is-a-much-longer-key-name","v")]),  # key-length term
    ("list-lp",     [("rpush","K","a","b","c","d")]),
    ("set-intset",  [("sadd","K","1","2","3","9")]),
    ("set-lp",      [("sadd","K","alpha","beta","gamma")]),
    ("hash-lp",     [("hset","K","f1","v1","f2","v2")]),
    ("zset-lp",     [("zadd","K","1","a","2","b")]),
]

div=0
def main():
    global div
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2]))
    for s in (od,fr):
        for k,v in [("set-max-intset-entries","512"),("set-max-listpack-entries","128"),
                    ("hash-max-listpack-entries","128"),("zset-max-listpack-entries","128"),
                    ("list-max-listpack-size","128")]:
            s.cmd("config","set",k,v)
    for label, build in KEYS:
        for db in (0, 2):
            for s in (od,fr): s.cmd("select",str(db)); s.cmd("flushall")
            for s in (od,fr):
                for cmd in build: s.cmd(*cmd)
            # the key name is build[0][1] unless it's the long-key case
            keyname = build[0][1] if label=="str-longkey" else "K"
            a=od.cmd("memory","usage",keyname); b=fr.cmd("memory","usage",keyname)
            if a!=b:
                div+=1; print(f"DIVERGE {label} db{db} MEMORY USAGE {keyname}: oracle={a} fr={b}")
        for s in (od,fr): s.cmd("select","0")
    if div: print(f"\nFAIL: {div} divergence(s)"); sys.exit(1)
    print(f"OK: MEMORY USAGE byte-exact across db0+db2 for {len(KEYS)} small-encoding keys vs redis 7.2.4")

if __name__=="__main__": main()
