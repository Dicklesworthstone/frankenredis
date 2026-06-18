#!/usr/bin/env python3
"""Differential gate for COPY (src dst [DB n] [REPLACE]) vs vendored redis 7.2.4.

COPY has no dedicated differ yet its semantics are non-trivial: dst-exists
without REPLACE returns 0 (no overwrite), REPLACE overwrites, missing src -> 0,
TTL is preserved on the copy, copy-to-self errors, cross-DB copy, and the value's
OBJECT ENCODING must be preserved (incl the a0p5p case: a listpack collection
built under a higher threshold then config-lowered must COPY as listpack, i.e.
the sticky force-flag is carried with the value, not re-derived). Diffs every
reply across all these.

Usage: copy_command_differ.py <oracle_port> <fr_port>   Exit 0=parity, 1=divergence.
"""
import socket, sys
def C(p): return socket.create_connection(("127.0.0.1",p),timeout=10)
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
        if t in (b'+',b':',b'-'): return l.decode()
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n).decode('latin1')
        if t in (b'*',b'~'): n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n)]
        return l.decode()
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else x
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()
def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2])); div=0
    def cleanup():
        for s in (od,fr):
            try:
                s.cmd("select","0"); s.cmd("config","set","hash-max-listpack-entries","512"); s.cmd("flushall")
            except Exception:
                pass
    def chk(label,*cmds):
        nonlocal div
        ao=[od.cmd(*c) for c in cmds]; af=[fr.cmd(*c) for c in cmds]
        if ao!=af:
            div+=1
            for i,c in enumerate(cmds):
                if ao[i]!=af[i]: print(f"DIVERGE {label} [{' '.join(c)}]\n  oracle: {ao[i]}\n  fr    : {af[i]}")
    for s in (od,fr): s.cmd("flushall")
    chk("basic",["set","a","hello"],["copy","a","b"],["get","b"])
    chk("exists-noreplace",["set","a","x"],["set","b","y"],["copy","a","b"],["get","b"])
    chk("exists-replace",["set","a","x"],["set","b","y"],["copy","a","b","REPLACE"],["get","b"])
    chk("missing-src",["copy","nope","b"])
    chk("ttl-preserved",["set","a","x","EX","500"],["copy","a","b"],["ttl","b"])
    chk("self",["set","a","x"],["copy","a","a"])
    chk("cross-db",["set","a","x"],["copy","a","b","DB","1"],["select","1"],["get","b"],["select","0"])
    chk("cross-db-existing-noreplace",
        ["select","0"],["flushall"],["set","src","new"],
        ["select","1"],["set","dst","old"],["select","0"],
        ["copy","src","dst","DB","1"],["select","1"],["get","dst"],["select","0"])
    chk("cross-db-existing-replace",
        ["select","0"],["flushall"],["set","src","new"],
        ["select","1"],["set","dst","old"],["select","0"],
        ["copy","src","dst","DB","1","REPLACE"],["select","1"],["get","dst"],["select","0"])
    chk("list-enc",["rpush","l","1","2","3"],["copy","l","l2"],["object","encoding","l2"],["lrange","l2","0","-1"])
    chk("hash-enc",["hset","h","f","v"],["copy","h","h2"],["object","encoding","h2"],["hgetall","h2"])
    chk("set-intset",["sadd","s","1","2","3"],["copy","s","s2"],["object","encoding","s2"])
    chk("zset",["zadd","z","1","a","2","b"],["copy","z","z2"],["object","encoding","z2"],["zrange","z2","0","-1"])
    for s in (od,fr):
        s.cmd("flushall"); s.cmd("config","set","hash-max-listpack-entries","300")
        for i in range(200): s.cmd("hset","bh","f%d"%i,"v%d"%i)
        s.cmd("config","set","hash-max-listpack-entries","128")
    chk("encoding-after-config-lower(a0p5p)",["object","encoding","bh"],["copy","bh","bh2"],["object","encoding","bh2"])
    print("-"*60)
    cleanup()
    if div: print(f"FAIL — {div} divergence(s)"); return 1
    print("PASS — COPY byte-exact vs redis 7.2.4 (incl encoding/TTL preservation + a0p5p case)"); return 0
if __name__=="__main__": sys.exit(main())
