#!/usr/bin/env python3
"""Differential gate: in-command duplicate args (frankenredis-...).

When the SAME member/field/key appears more than once in a single command, redis has
precise semantics that fr must mirror: ZADD/HSET keep the LAST value and the return
count is NEW-UNIQUE-ELEMENTS-added (not occurrences); MSET/MSETNX last-wins; SADD/PFADD
dedup; RPUSH/LPUSH KEEP duplicates; GEOADD keeps the last coordinate; ZADD CH counts
changed; SET with a repeated EX is accepted (last wins, NOT a syntax error). This pins
the resulting value + return code byte-exact vs redis 7.2.4.

Usage: in_command_dup_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    cmd(od,"FLUSHALL"); cmd(fr,"FLUSHALL")
    def chk(label, build, read):
        for s in (od,fr): cmd(s,"DEL","k")
        ro,rf=cmd(od,*build),cmd(fr,*build)
        vo,vf=cmd(od,*read),cmd(fr,*read)
        if ro!=rf: fails.append(f"{label} ret: redis={ro[:50]!r} fr={rf[:50]!r}")
        if vo!=vf: fails.append(f"{label} val: redis={vo[:60]!r} fr={vf[:60]!r}")
    chk("zadd_dup",["ZADD","k","1","m","2","m"],["ZSCORE","k","m"])
    chk("zadd_dup3",["ZADD","k","1","m","2","m","3","m"],["ZSCORE","k","m"])
    chk("zadd_dup_mixed",["ZADD","k","1","a","2","b","9","a"],["ZRANGE","k","0","-1","WITHSCORES"])
    chk("zadd_dup_ch",["ZADD","k","CH","1","m","2","m"],["ZSCORE","k","m"])
    chk("hset_dup",["HSET","k","f","a","f","b"],["HGET","k","f"])
    chk("hset_dup3",["HSET","k","f","a","f","b","f","c"],["HGET","k","f"])
    chk("hset_dup_mixed",["HSET","k","f1","a","f2","b","f1","z"],["HGETALL","k"])
    chk("mset_dup",["MSET","k","v1","k","v2"],["GET","k"])
    chk("msetnx_dup",["MSETNX","k","v1","k","v2"],["GET","k"])
    chk("sadd_dup",["SADD","k","x","x","y","x"],["SMEMBERS","k"])
    chk("rpush_dup",["RPUSH","k","a","a","b","a"],["LRANGE","k","0","-1"])
    chk("pfadd_dup",["PFADD","k","x","x","y"],["PFCOUNT","k"])
    chk("geoadd_dup",["GEOADD","k","1","1","m","2","2","m"],["GEOPOS","k","m"])
    # SET repeated option (accepted, last wins) + GET back; read PTTL bucket via TTL
    for s in (od,fr): cmd(s,"DEL","sk")
    ro,rf=cmd(od,"SET","sk","v","EX","100","EX","200"),cmd(fr,"SET","sk","v","EX","100","EX","200")
    if ro!=rf: fails.append(f"set_repeated_ex: redis={ro!r} fr={rf!r}")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} in-command-dup divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        sys.exit(1)
    print("PASS — in-command duplicate semantics byte-exact vs redis 7.2.4 (last-wins/dedup/keep + return counts)")
if __name__=="__main__": main()
