#!/usr/bin/env python3
"""Differential gate: range-index i64-extreme handling (frankenredis-8jqlx).

Range commands normalize negative indices via `start + len` and clamp to bounds; at
i64 max/min (and overflow-string args) that arithmetic can wrap/saturate differently
between impls (the g3ioa arithmetic-boundary class). This pins GETRANGE / LRANGE /
ZRANGE / LTRIM / ZREMRANGEBYRANK / SETRANGE with i64 max, i64 min, i64-max+1,
20-digit overflow, and ordinary indices, byte-exact vs redis 7.2.4.

Usage: range_index_extreme_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.015); return s.recv(1<<20)
IMAX="9223372036854775807"; IMIN="-9223372036854775808"
IMAXP1="9223372036854775808"; HUGE="99999999999999999999"
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    def reset():
        for s in (od,fr):
            cmd(s,"FLUSHALL"); cmd(s,"SET","str","Hello World")
            cmd(s,"RPUSH","lst","a","b","c","d","e")
            cmd(s,"ZADD","z","1","a","2","b","3","c","4","d","5","e")
    def chk(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        if ro!=rf: fails.append(f"{label}: redis={ro!r} fr={rf!r}")
    reset()
    ext=[IMAX,IMIN,IMAXP1,"-"+IMAXP1,HUGE,"-"+HUGE]
    for a in ext:
        for b in [IMAX,IMIN,"0","-1"]:
            chk(f"getrange[{a},{b}]","GETRANGE","str",a,b)
            chk(f"lrange[{a},{b}]","LRANGE","lst",a,b)
            chk(f"zrange[{a},{b}]","ZRANGE","z",a,b)
    # mutating range ops (reset each)
    for a in [IMAX,IMIN,"0","-100"]:
        for b in [IMAX,IMIN,"-1"]:
            for s in (od,fr): cmd(s,"DEL","lt"); cmd(s,"RPUSH","lt","a","b","c")
            chk(f"ltrim[{a},{b}]","LTRIM","lt",a,b); chk(f"ltrim_res[{a},{b}]","LRANGE","lt","0","-1")
            for s in (od,fr): cmd(s,"DEL","zr"); cmd(s,"ZADD","zr","1","a","2","b","3","c")
            chk(f"zremrank[{a},{b}]","ZREMRANGEBYRANK","zr",a,b); chk(f"zremrank_res[{a},{b}]","ZRANGE","zr","0","-1")
    for a in [IMAX,HUGE,IMIN,IMAXP1]:
        chk(f"setrange_off[{a}]","SETRANGE","s2",a,"x")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} range-index i64-extreme divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        sys.exit(1)
    print("PASS — range-index i64-extreme handling byte-exact vs redis 7.2.4 (GETRANGE/LRANGE/ZRANGE/LTRIM/ZREMRANGEBYRANK/SETRANGE)")
if __name__=="__main__": main()
