#!/usr/bin/env python3
"""Differential gate: RESP3 nested-aggregate reply structure (frankenredis-...).

Under HELLO 3, withscores/entry replies change NESTING (not just the leading type
byte): ZRANGE/ZRANGEBYSCORE/ZPOPMIN/ZPOPMAX WITHSCORES return an array of 2-element
[member, Double] PAIRS (RESP2 returns a flat member,score,member,score array); a
single ZPOPMIN returns a flat [member, Double]; ZMPOP returns [key, [[member,
Double]...]]; ZSCORE returns a bare Double; HGETALL/CONFIG GET/XREAD return Maps;
XRANGE returns [[id, [field,value...]]...]. This pins the full byte structure (not
just the type tag, which resp3_type_tag_differ covers) for deterministic full-set
queries vs redis 7.2.4. (RESP3 score emission has produced real bugs — ZRANK
WITHSCORE, BZPOPMIN.)

Usage: resp3_nested_aggregate_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p):
    s=socket.create_connection(("127.0.0.1",p),timeout=5); s.settimeout(1.2); return s
def send(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o)
def rd(s):
    time.sleep(0.05)
    try: return s.recv(1<<20)
    except Exception: return b""
def cmd(s,*a): send(s,*a); return rd(s)
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    cmd(od,"HELLO","3"); cmd(fr,"HELLO","3")
    def seed():
        for s in (od,fr):
            cmd(s,"FLUSHALL")
            cmd(s,"ZADD","z","1","a","2","b","3","c")
            cmd(s,"HSET","h","f1","v1","f2","v2")
            cmd(s,"XADD","st","1-1","fa","va"); cmd(s,"XADD","st","2-2","fb","vb")
            cmd(s,"ZADD","z2","5","a")
    def chk(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        if ro!=rf: fails.append(f"{label}: redis={ro[:90]!r} fr={rf[:90]!r}")
    seed()
    chk("zrange_ws","ZRANGE","z","0","-1","WITHSCORES")
    chk("zrange_rev_ws","ZRANGE","z","0","-1","REV","WITHSCORES")
    chk("zrangebyscore_ws","ZRANGEBYSCORE","z","-inf","+inf","WITHSCORES")
    chk("zrevrangebyscore_ws","ZREVRANGEBYSCORE","z","+inf","-inf","WITHSCORES")
    chk("zscore_double","ZSCORE","z","a")
    chk("zmscore","ZMSCORE","z","a","nope","c")
    chk("zdiff_ws","ZDIFF","2","z","z2","WITHSCORES")
    chk("zunion_ws","ZUNION","2","z","z2","WITHSCORES")
    chk("zpopmin_single","ZPOPMIN","z")
    seed(); chk("zpopmin_count","ZPOPMIN","z","3")
    seed(); chk("zpopmax_count","ZPOPMAX","z","2")
    seed(); chk("zmpop","ZMPOP","1","z","MIN","COUNT","2")
    chk("hgetall_map","HGETALL","h")
    chk("xrange","XRANGE","st","-","+")
    chk("xread_map","XREAD","COUNT","10","STREAMS","st","0")
    chk("config_map","CONFIG","GET","maxmemory")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} RESP3 nested-aggregate divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        sys.exit(1)
    print("PASS — RESP3 nested-aggregate structure byte-exact vs redis 7.2.4 (WITHSCORES [member,Double] pairs, ZMPOP nesting, Maps, stream entries)")
if __name__=="__main__": main()
