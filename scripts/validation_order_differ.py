#!/usr/bin/env python3
"""Differential gate: command validation ORDER / multi-error precedence (frankenredis-...).

When a command hits several errors at once, redis reports a SPECIFIC one first; the
order is subtle and a known bug vein (6jcwp = GETRANGE wrongtype-vs-empty). This pins
30 deterministic multi-error cases byte-exact vs redis 7.2.4: SETRANGE/SETBIT/LPOP
validate the numeric arg (offset/bit-value/count) range BEFORE the WRONGTYPE check;
ZADD validates the flag-syntax and score-parse before the type; incompatible flag
combos (ZADD GT+NX, EXPIRE NX+XX / GT+LT, SET EX+PX / NX+XX, GETEX EX+PERSIST) emit
their exact messages; INCR reports not-integer vs WRONGTYPE correctly.

Usage: validation_order_differ.py <oracle_port> <fr_port>
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
    for s in (od,fr):
        cmd(s,"FLUSHALL"); cmd(s,"RPUSH","listk","a","b","c"); cmd(s,"SET","strk","hello")
        cmd(s,"ZADD","zk","1","a"); cmd(s,"HSET","hk","f","v")
    def chk(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        if ro!=rf: fails.append(f"{label}: redis={ro[:70]!r} fr={rf[:70]!r}")
    chk("setrange_wt_negoff","SETRANGE","listk","-1","x")
    chk("getrange_wt","GETRANGE","listk","0","1")
    chk("setbit_wt_badbit","SETBIT","listk","0","5")
    chk("setbit_wt_negoff","SETBIT","listk","-1","1")
    chk("incr_wt","INCR","listk")
    chk("incr_nonint","INCR","strk")
    chk("getbit_wt_negoff","GETBIT","listk","-1")
    chk("strlen_wt","STRLEN","listk")
    chk("append_wt","APPEND","listk","x")
    chk("zadd_wt_badflag","ZADD","listk","BADFLAG","1","m")
    chk("zadd_gt_nx","ZADD","zk","GT","NX","1","a")
    chk("zadd_nan","ZADD","zk","nan","m")
    chk("zrangebyscore_wt","ZRANGEBYSCORE","listk","0","1")
    chk("zadd_wt_nan","ZADD","listk","nan","m")
    chk("hset_wt","HSET","listk","f","v")
    chk("hincrby_wt","HINCRBY","listk","f","1")
    chk("lpush_wt","LPUSH","strk","x")
    chk("lpop_wt_negcount","LPOP","strk","-1")
    chk("lset_wt","LSET","strk","0","x")
    chk("linsert_wt","LINSERT","strk","BEFORE","a","b")
    chk("sinterstore_wt_src","SINTERSTORE","dst","listk","strk")
    chk("sadd_wt","SADD","strk","x")
    chk("expire_nx_xx","EXPIRE","strk","100","NX","XX")
    chk("expire_gt_lt","EXPIRE","strk","100","GT","LT")
    chk("expire_badflag","EXPIRE","strk","100","ZZ")
    chk("getex_ex_persist","GETEX","strk","EX","100","PERSIST")
    chk("set_ex_px","SET","k2","v","EX","100","PX","100")
    chk("set_nx_xx","SET","k2","v","NX","XX")
    cmd(od,"HSET","hk","n","abc"); cmd(fr,"HSET","hk","n","abc")
    chk("hincrbyfloat_nonfloat","HINCRBYFLOAT","hk","n","1.5")
    chk("incrbyfloat_nonfloat","INCRBYFLOAT","strk","1.5")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} validation-order divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        sys.exit(1)
    print(f"PASS — validation-order / multi-error precedence byte-exact vs redis 7.2.4 (30 cases)")
if __name__=="__main__": main()
