#!/usr/bin/env python3
"""Differential gate: *STORE/BITOP/COPY destination-aliases-source (frankenredis-...).

When a writing command's DESTINATION is also one of its SOURCE keys, redis computes
the full result BEFORE overwriting the destination (no mid-computation clobber). fr
must match: SINTERSTORE/SUNIONSTORE/SDIFFSTORE, ZUNIONSTORE/ZINTERSTORE/ZDIFFSTORE,
SORT ... STORE, ZRANGESTORE, and BITOP with dest==a source produce the same value +
count as redis; SDIFFSTORE of a key against itself yields empty and DELETES the dest;
COPY with src==dst errors. Byte-exact vs redis 7.2.4. (store_dest_semantics_differ
covers *STORE generally; this is the aliasing class incl. BITOP/COPY.)

Usage: store_dest_aliasing_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time, re
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def sortmem(b): return tuple(sorted(re.findall(rb"\$\d+\r\n([^\r]*)\r\n", b)))
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    def setup():
        for s in (od,fr):
            cmd(s,"FLUSHALL")
            cmd(s,"SADD","sa","1","2","3","4"); cmd(s,"SADD","sb","3","4","5","6")
            cmd(s,"ZADD","za","1","a","2","b","3","c"); cmd(s,"ZADD","zb","10","b","20","c","30","d")
            cmd(s,"RPUSH","lst","3","1","2")
            cmd(s,"SET","b1","abc"); cmd(s,"SET","b2","ABCD"); cmd(s,"SET","str","hello")
    def chk(label, build, read, sortread=False):
        ro,rf=cmd(od,*build),cmd(fr,*build)
        vo,vf=cmd(od,*read),cmd(fr,*read)
        if sortread: vo,vf=sortmem(vo),sortmem(vf)
        if ro!=rf: fails.append(f"{label} ret: redis={ro[:50]!r} fr={rf[:50]!r}")
        if vo!=vf: fails.append(f"{label} val: redis={str(vo)[:60]} fr={str(vf)[:60]}")
    setup(); chk("sinterstore_self",["SINTERSTORE","sa","sa","sb"],["SMEMBERS","sa"],True)
    setup(); chk("sunionstore_self",["SUNIONSTORE","sa","sa","sb"],["SMEMBERS","sa"],True)
    setup(); chk("sdiffstore_self",["SDIFFSTORE","sa","sa","sb"],["SMEMBERS","sa"],True)
    setup(); chk("sinterstore_2nd",["SINTERSTORE","sb","sa","sb"],["SMEMBERS","sb"],True)
    setup(); chk("sinter_self_only",["SINTERSTORE","sa","sa"],["SMEMBERS","sa"],True)
    setup(); chk("sdiffstore_allself",["SDIFFSTORE","sa","sa","sa"],["EXISTS","sa"])
    setup(); chk("zunionstore_self",["ZUNIONSTORE","za","2","za","zb"],["ZRANGE","za","0","-1","WITHSCORES"])
    setup(); chk("zinterstore_self",["ZINTERSTORE","za","2","za","zb"],["ZRANGE","za","0","-1","WITHSCORES"])
    setup(); chk("zdiffstore_self",["ZDIFFSTORE","za","2","za","zb"],["ZRANGE","za","0","-1","WITHSCORES"])
    setup(); chk("zunion_self_weights",["ZUNIONSTORE","za","2","za","zb","WEIGHTS","2","3"],["ZRANGE","za","0","-1","WITHSCORES"])
    setup(); chk("sort_store_self",["SORT","lst","STORE","lst"],["LRANGE","lst","0","-1"])
    setup(); chk("zrangestore_self",["ZRANGESTORE","za","za","0","-1"],["ZRANGE","za","0","-1","WITHSCORES"])
    setup(); chk("bitop_and_self",["BITOP","AND","b1","b1","b2"],["GET","b1"])
    setup(); chk("bitop_xor_self",["BITOP","XOR","b1","b1","b2"],["GET","b1"])
    setup(); chk("copy_self",["COPY","str","str","REPLACE"],["GET","str"])
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} dest-aliasing divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        sys.exit(1)
    print("PASS — *STORE/BITOP/COPY dest-aliases-source byte-exact vs redis 7.2.4 (compute-then-store, self-diff deletes, copy-self errors)")
if __name__=="__main__": main()
