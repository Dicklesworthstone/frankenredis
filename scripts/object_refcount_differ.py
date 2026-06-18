#!/usr/bin/env python3
"""Differential gate: OBJECT REFCOUNT shared-integer modeling (frankenredis-...).

redis caches the integers 0..9999 as shared objects with a sentinel refcount
(2147483647 = OBJ_SHARED_REFCOUNT); all other values (large ints, strings, non-string
types) report 1. Integer sharing is DISABLED when maxmemory is set with an eviction
policy (the LRU/LFU field would be unusable on a shared object), so a small int then
reports 1. This pins fr's refcount model byte-exact vs redis 7.2.4 across the shared
range, the 9999/10000 boundary (incl. INCR crossing it), non-shared types, and the
maxmemory-disables-sharing case. Restores maxmemory=0/noeviction (suite-safe).

Usage: object_refcount_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def short(r): return r[:r.index(b"\r\n")] if b"\r\n" in r else r[:40]
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    def each(*c):
        for s in (od,fr): cmd(s,*c)
    def chk(label,*c):
        ro,rf=short(cmd(od,*c)),short(cmd(fr,*c))
        if ro!=rf: fails.append(f"{label}: redis={ro!r} fr={rf!r}")
    try:
        each("FLUSHALL"); each("CONFIG","SET","maxmemory","0"); each("CONFIG","SET","maxmemory-policy","noeviction")
        for k,v in [("z","0"),("small","100"),("max9999","9999")]:
            each("SET",k,v); chk(f"shared_{k}","OBJECT","REFCOUNT",k)
        for k,v in [("over","10000"),("big","100000"),("longnum","12345678901234567890")]:
            each("SET",k,v); chk(f"nonshared_{k}","OBJECT","REFCOUNT",k)
        each("SET","strv","hello"); chk("string","OBJECT","REFCOUNT","strv")
        each("APPEND","app","x"); chk("appended_raw","OBJECT","REFCOUNT","app")
        each("RPUSH","lst","a"); chk("list","OBJECT","REFCOUNT","lst")
        each("SET","c1","9998"); each("INCR","c1"); chk("incr_to_9999","OBJECT","REFCOUNT","c1")
        each("SET","c2","9999"); each("INCR","c2"); chk("incr_to_10000","OBJECT","REFCOUNT","c2")
        # maxmemory + eviction disables int sharing
        each("CONFIG","SET","maxmemory","100mb"); each("CONFIG","SET","maxmemory-policy","allkeys-lru")
        each("SET","sh","50"); chk("maxmem_no_share","OBJECT","REFCOUNT","sh")
    finally:
        each("CONFIG","SET","maxmemory","0"); each("CONFIG","SET","maxmemory-policy","noeviction")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} OBJECT REFCOUNT divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        sys.exit(1)
    print("PASS — OBJECT REFCOUNT shared-integer model byte-exact vs redis 7.2.4 (0..9999 shared, boundary, non-shared types, maxmemory-disables-sharing)")
if __name__=="__main__": main()
