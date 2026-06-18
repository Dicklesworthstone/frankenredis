#!/usr/bin/env python3
"""Differential gate: COPY/RENAME/MOVE encoding preservation (frankenredis-...).

COPY/RENAME/MOVE duplicate or relocate a key IN MEMORY — redis preserves the source
object's encoding verbatim (no content-based re-derivation). So a set that grew to
`hashtable` then shrank (sticky) stays `hashtable` after COPY/RENAME/MOVE. This is the
MIRROR of bbyfz (the RESTORE/RDB-load path, which DOES re-derive): here the correct
behavior is preserve-not-rederive, and fr matches. Covers sticky set + hash, RENAME,
MOVE cross-db, and COPY ... DB.

Usage: copy_rename_encoding_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
CFG = {"set-max-intset-entries":"512","set-max-listpack-entries":"128",
       "hash-max-listpack-entries":"128","zset-max-listpack-entries":"128",
       "list-max-listpack-size":"128"}
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def enc(s,k):
    r=cmd(s,"OBJECT","ENCODING",k); return r[r.index(b"\r\n")+2:].split(b"\r\n")[0] if r.startswith(b"$") and b"$-1" not in r[:4] else b"?"
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    for s in (od,fr):
        cmd(s,"SELECT","1"); cmd(s,"FLUSHDB"); cmd(s,"SELECT","0"); cmd(s,"FLUSHDB")
        for k,v in CFG.items(): cmd(s,"CONFIG","SET",k,v)
    def chk(label,key,db=None):
        if db is not None:
            for s in (od,fr): cmd(s,"SELECT",str(db))
        eo,ef=enc(od,key),enc(fr,key)
        if db is not None:
            for s in (od,fr): cmd(s,"SELECT","0")
        if eo!=ef: fails.append(f"{label}: redis={eo.decode()} fr={ef.decode()}")
    def sticky_set(s,key):
        cmd(s,"DEL",key)
        for i in range(0,200,50): cmd(s,"SADD",key,*[f"m{j}" for j in range(i,i+50)])
        for i in range(2,200): cmd(s,"SREM",key,f"m{i}")
    def sticky_hash(s,key):
        cmd(s,"DEL",key)
        for i in range(0,200,50): cmd(s,"HSET",key,*sum([[f"f{j}",f"v{j}"] for j in range(i,i+50)],[]))
        for i in range(2,200): cmd(s,"HDEL",key,f"f{i}")
    for s in (od,fr): sticky_set(s,"sset"); cmd(s,"COPY","sset","sset_c")
    chk("copy_set_src","sset"); chk("copy_set_dst","sset_c")
    for s in (od,fr): sticky_hash(s,"shash"); cmd(s,"COPY","shash","shash_c")
    chk("copy_hash_dst","shash_c")
    for s in (od,fr): sticky_set(s,"rset"); cmd(s,"RENAME","rset","rset2")
    chk("rename_set","rset2")
    for s in (od,fr): sticky_set(s,"mset"); cmd(s,"MOVE","mset","1")
    chk("move_set_db1","mset",db=1)
    for s in (od,fr): cmd(s,"SELECT","0"); sticky_set(s,"cset"); cmd(s,"COPY","cset","cset_d1","DB","1")
    chk("copy_db_set","cset_d1",db=1)
    for s in (od,fr):
        cmd(s,"SELECT","1"); cmd(s,"FLUSHDB"); cmd(s,"SELECT","0")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} COPY/RENAME/MOVE encoding-preservation divergence(s):")
        for x in fails[:8]: print(f"  {x}")
        sys.exit(1)
    print("PASS — COPY/RENAME/MOVE preserve sticky encoding byte-exact vs redis 7.2.4 (mirror of bbyfz RESTORE re-derive)")
if __name__=="__main__": main()
