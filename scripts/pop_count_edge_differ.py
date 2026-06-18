#!/usr/bin/env python3
"""Differential gate: pop-command count-argument edges (frankenredis-...).

LPOP/RPOP/SPOP/ZPOPMIN/ZPOPMAX with a count argument have subtle, divergent-prone
edges: count=0 -> empty aggregate (*0); count>size -> all elements; negative/float
count -> "ERR value is out of range, must be positive"; and the MISSING-key reply
DIFFERS by command — LPOP/RPOP on a missing key with count return a NULL array (*-1)
while SPOP/ZPOPMIN/ZPOPMAX return an EMPTY array (*0); SPOP/ZPOP without count on a
missing key return a null bulk ($-1) / empty. This pins all of that byte-exact vs
redis 7.2.4. SPOP-count results are compared as a SORTED multiset (member order is
random); LPOP/RPOP/ZPOP{MIN,MAX} are fully ordered so compared exactly.

Usage: pop_count_edge_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import re, socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def sortbulks(b): return (b[:4], tuple(sorted(re.findall(rb"\$\d+\r\n([^\r]*)\r\n", b))))
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    IMAX="9223372036854775807"; IMIN="-9223372036854775808"
    def setup():
        for s in (od,fr):
            cmd(s,"FLUSHALL"); cmd(s,"RPUSH","l","a","b","c")
            cmd(s,"SADD","st","x","y","z"); cmd(s,"ZADD","z","1","a","2","b","3","c")
    def chk(label,*c,sortset=False):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        a,b=(sortbulks(ro),sortbulks(rf)) if sortset else (ro,rf)
        if a!=b: fails.append(f"{label}: redis={ro[:60]!r} fr={rf[:60]!r}")
    setup()
    chk("lpop_0","LPOP","l","0"); chk("lpop_gt","LPOP","l","100")
    setup(); chk("lpop_neg","LPOP","l","-1"); chk("lpop_imin","LPOP","l",IMIN)
    chk("lpop_missing_count","LPOP","nope","2"); chk("lpop_missing_count0","LPOP","nope","0")
    chk("lpop_float","LPOP","l","1.5")
    setup(); chk("rpop_0","RPOP","l","0"); chk("rpop_imax","RPOP","l",IMAX)
    setup()
    chk("spop_0","SPOP","st","0"); chk("spop_gt","SPOP","st","100",sortset=True)
    setup(); chk("spop_neg","SPOP","st","-1"); chk("spop_float","SPOP","st","abc")
    chk("spop_missing_count","SPOP","nope","2"); chk("spop_missing_nocount","SPOP","nope")
    setup()
    chk("zpopmin_0","ZPOPMIN","z","0"); chk("zpopmin_gt","ZPOPMIN","z","100")
    setup(); chk("zpopmax_gt","ZPOPMAX","z","100")
    setup(); chk("zpopmin_neg","ZPOPMIN","z","-1"); chk("zpopmin_float","ZPOPMIN","z","2.0")
    chk("zpopmin_missing","ZPOPMIN","nope"); chk("zpopmin_missing_count","ZPOPMIN","nope","3")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} pop count-edge divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        sys.exit(1)
    print("PASS — pop-command count edges byte-exact vs redis 7.2.4 (count=0/>size/neg/float/missing; LPOP-nil vs SPOP/ZPOP-empty distinction)")
if __name__=="__main__": main()
