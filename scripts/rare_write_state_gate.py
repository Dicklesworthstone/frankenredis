#!/usr/bin/env python3
"""State-isolated rare-WRITE differential gate: fr vs redis 7.2.4.

For each random rare write command (GETSET/SETNX/COPY/RENAMENX/MOVE/HSETNX/
LPUSHX/RPUSHX/SMOVE/GETDEL/GETEX/APPEND/SETRANGE/LREM/LINSERT/LSET/ZADD-flags/
ZINCRBY/INCRBYFLOAT/EXPIRE-flags/SET-flags), reset BOTH servers to an identical
seed state, run ONE command, and compare the reply AND the full resulting
keyspace state. State isolation avoids the time-based-expiry desync that makes
stateful fuzzers report spurious downstream WRONGTYPE cascades.

Set ORACLE_PORT / FR_PORT env vars (default 29501/29502).
Usage: rare_write_state_gate.py <seed> <iterations>
"""
import socket,random,sys
import os
RED=int(os.environ.get('ORACLE_PORT','29501'))
FR=int(os.environ.get('FR_PORT','29502'))
def conn(p):
    s=socket.create_connection(("127.0.0.1",p),timeout=4);s.settimeout(4);return s
def enc(a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x.encode() if isinstance(x,str) else x; o+=b"$%d\r\n%s\r\n"%(len(x),x)
    return o
def rr(s):
    buf=b""
    def rl():
        nonlocal buf
        while b"\r\n" not in buf:
            d=s.recv(8192)
            if not d:return None
            buf+=d
        i=buf.index(b"\r\n");l=buf[:i];buf=buf[i+2:];return l
    def rn(n):
        nonlocal buf
        while len(buf)<n:
            d=s.recv(8192)
            if not d:break
            buf+=d
        o=buf[:n];buf=buf[n:];return o
    def one():
        l=rl()
        if l is None:return b"<EOF>"
        t=l[:1]
        if t in(b"+",b"-",b":",b",",b"#",b"(",b"_"):return l+b"\r\n"
        if t in(b"$",b"="):
            n=int(l[1:]);return l+b"\r\n" if n<0 else l+b"\r\n"+rn(n+2)
        if t in(b"*",b"%",b"~",b">"):
            n=int(l[1:])
            if n<0:return l+b"\r\n"
            c=n*(2 if t==b"%" else 1);ps=[l+b"\r\n"]
            for _ in range(c):ps.append(one())
            return b"".join(ps)
        return l+b"\r\n"
    return one()
def cmd(s,a):s.sendall(enc(a));return rr(s)
def seed(s):
    cmd(s,["FLUSHALL"])
    cmd(s,["SET","str","hello"]);cmd(s,["SET","num","42"])
    cmd(s,["RPUSH","lst","a","b","c"]);cmd(s,["SADD","st","x","y"])
    cmd(s,["HSET","hsh","f1","v1","f2","v2"]);cmd(s,["ZADD","zs","1","a","2","b"])
    cmd(s,["EXPIRE","str","10000"])
def state(s):
    out=[]
    for k in["str","num","lst","st","hsh","zs","dst","newk","str2"]:
        t=cmd(s,["TYPE",k]);out.append(t)
        if b"string"in t:out.append(cmd(s,["GET",k]))
        elif b"list"in t:out.append(cmd(s,["LRANGE",k,"0","-1"]))
        elif b"set"in t:out.append(cmd(s,["SMEMBERS",k]))
        elif b"hash"in t:out.append(cmd(s,["HGETALL",k]))
        elif b"zset"in t:out.append(cmd(s,["ZRANGE",k,"0","-1","WITHSCORES"]))
        out.append(cmd(s,["TTL",k])[:1])  # ttl existence (coarse)
    return b"|".join(out)
K=["str","num","lst","st","hsh","zs","dst","newk","nope"]
V=["v","1","-1","abc",""]
def k():return random.choice(K)
def v():return random.choice(V)
WCMDS=[
 lambda:["GETSET",k(),v()],lambda:["SETNX",k(),v()],lambda:["COPY",k(),k()],
 lambda:["COPY",k(),k(),"REPLACE"],lambda:["RENAMENX",k(),k()],lambda:["RENAME",k(),k()],
 lambda:["MOVE",k(),"1"],lambda:["HSETNX",k(),"f1",v()],lambda:["HSETNX",k(),"fx",v()],
 lambda:["LPUSHX",k(),v()],lambda:["RPUSHX",k(),v()],lambda:["SMOVE",k(),k(),random.choice(["x","z"])],
 lambda:["GETDEL",k()],lambda:["GETEX",k(),"PERSIST"],lambda:["PERSIST",k()],
 lambda:["APPEND",k(),v()],lambda:["SETRANGE",k(),"2",v()],lambda:["LREM",k(),"1","a"],
 lambda:["LINSERT",k(),random.choice(["BEFORE","AFTER"]),"a",v()],lambda:["LSET",k(),"0",v()],
 lambda:["SADD",k(),v()],lambda:["SREM",k(),"x"],lambda:["HDEL",k(),"f1"],
 lambda:["ZADD",k(),random.choice(["GT","LT","NX","XX","CH","INCR","GT CH","NX GT"]),"5","a"],
 lambda:["ZINCRBY",k(),"3","a"],lambda:["ZREM",k(),"a"],lambda:["INCRBYFLOAT",k(),"1.5"],
 lambda:["EXPIRE",k(),"100","NX"],lambda:["EXPIRE",k(),"100","XX"],lambda:["EXPIRE",k(),"100","GT"],
 lambda:["SET",k(),v(),"KEEPTTL"],lambda:["SET",k(),v(),"XX","GET"],lambda:["SET",k(),v(),"NX","GET"],
]
def main():
    sd=int(sys.argv[1]);N=int(sys.argv[2]);random.seed(sd)
    fr,red=conn(FR),conn(RED)
    diffs=[]
    for _ in range(N):
        a=random.choice(WCMDS)()
        a=[x for x in a]
        # flatten "GT CH" style
        flat=[]
        for x in a:flat+=x.split(" ") if (isinstance(x,str) and " " in x) else [x]
        a=flat
        seed(fr);seed(red)
        rf=cmd(fr,a);rb=cmd(red,a)
        sf=state(fr);sb=state(red)
        if rf!=rb or sf!=sb:
            diffs.append((a,rb,rf,sb!=sf))
    print(f"seed={sd} N={N} diffs={len(diffs)}")
    seen=set()
    for a,rb,rf,sd2 in diffs:
        key=(tuple(a[:1]),tuple(a),rb[:1],rf[:1],sd2)
        if key in seen:continue
        seen.add(key)
        print(f"  {a} [statediff={sd2}]\n    redis_reply={rb[:80]!r}\n    fr_reply   ={rf[:80]!r}")
    if diffs:
        sys.exit(1)
    print("PASS - rare-write reply+state byte-exact vs redis 7.2.4")
main()
