#!/usr/bin/env python3
"""Differential gate: RESTORE/RDB-load encoding re-optimization (frankenredis-bbyfz).

When a collection grows past its threshold (-> hashtable/skiplist/quicklist) then
shrinks, redis keeps the upgraded encoding in memory (sticky); but on DUMP+RESTORE
(or RDB reload) redis RE-DERIVES the optimal encoding for the now-small content
(listpack/intset). fr does this correctly for HASH, ZSET, and LIST; this gate locks
that. SET is the documented exception (frankenredis-bbyfz: a RESTORE'd small
RDB_TYPE_SET stays hashtable on fr vs listpack/intset on redis) — the set cases are
checked here as a KNOWN-divergence report (not asserted) so the gate stays green and
flips to a real assertion once bbyfz is fixed.

bbyfz is DUAL-PATH: the fr-runtime RDB-file-load path (apply_rdb_entries,
RdbValue::SetHashtable) was fixed by nom8d (re-derives; forces hashtable only when
len > set-max-intset-entries), but the RESTORE command's fr-store deserialize path
(fr-store/src/lib.rs:20694) still UNCONDITIONALLY force_set_hashtable_encoding=true
for RDB_TYPE_SET. This gate (DUMP+RESTORE) exercises the still-buggy path; a
DEBUG-RELOAD-based gate would exercise the already-fixed one.

Usage: restore_reoptimize_encoding_differ.py <oracle_port> <fr_port>
       Exit 0 = hash/zset/list re-optimization byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=8)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.01); return s.recv(1<<20)
def enc(s,k):
    r=cmd(s,"OBJECT","ENCODING",k); return r[r.index(b"\r\n")+2:].split(b"\r\n")[0] if r.startswith(b"$") and b"$-1" not in r[:4] else b"?"
def dump(s,key): r=cmd(s,"DUMP",key); nl=r.index(b"\r\n"); return r[nl+2:nl+2+int(r[1:nl])]
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]; known=[]
    for s in (od,fr):
        cmd(s,"FLUSHALL")
        for c in ["hash-max-listpack-entries","zset-max-listpack-entries","set-max-listpack-entries"]: cmd(s,"CONFIG","SET",c,"128")
        cmd(s,"CONFIG","SET","set-max-intset-entries","512"); cmd(s,"CONFIG","SET","list-max-listpack-size","128")
    def grew_shrunk(s,kind):
        cmd(s,"DEL","g")
        if kind=="hash":
            for i in range(0,200,50): cmd(s,"HSET","g",*sum([[f"f{j}",f"v{j}"] for j in range(i,i+50)],[]))
            for i in range(2,200): cmd(s,"HDEL","g",f"f{i}")
        elif kind=="zset":
            for i in range(0,200,50): cmd(s,"ZADD","g",*sum([[str(j),f"m{j}"] for j in range(i,i+50)],[]))
            for i in range(2,200): cmd(s,"ZREM","g",f"m{i}")
        elif kind=="list":
            for i in range(200): cmd(s,"RPUSH","g",f"e{i}")
            for _ in range(198): cmd(s,"LPOP","g")
        elif kind=="set":
            for i in range(0,200,50): cmd(s,"SADD","g",*[f"m{j}" for j in range(i,i+50)])
            for i in range(2,200): cmd(s,"SREM","g",f"m{i}")
        elif kind=="intset":
            for i in range(600): cmd(s,"SADD","g",str(i))
            for i in range(2,600): cmd(s,"SREM","g",str(i))
    def check(kind, assert_it):
        for s in (od,fr): grew_shrunk(s,kind)
        pl=dump(od,"g")
        for s in (od,fr): cmd(s,"DEL","R"); cmd(s,"RESTORE","R","0",pl)
        ro,rf=enc(od,"R"),enc(fr,"R")
        if ro!=rf:
            (fails if assert_it else known).append(f"{kind}: redis={ro.decode()} fr={rf.decode()}")
    for kind in ("hash","zset","list","set","intset"):
        check(kind, True)   # set/intset HARD-asserted since bbyfz fix (fc0fe5212)
    print("="*60)
    if known:
        print("KNOWN (frankenredis-bbyfz, not asserted): " + "; ".join(known))
    if fails:
        print(f"FAIL — {len(fails)} restore re-optimization divergence(s) vs redis 7.2.4:")
        for x in fails[:8]: print(f"  {x}")
        sys.exit(1)
    print("PASS — hash/zset/list/SET/intset RESTORE encoding re-optimization byte-exact vs redis 7.2.4 (set/intset hard-asserted post-bbyfz fix)")
if __name__=="__main__": main()
