#!/usr/bin/env python3
"""Time compute/large-data commands fr vs redis 7.2.4 to find a real gap.
Populates large keys once, then times N iterations of each read/compute command.
"""
import socket, sys, time
def conn(p):
    s=socket.create_connection(("127.0.0.1",p),timeout=30); s.settimeout(30); return s
class R:
    def __init__(s,p): s.s=conn(p); s.buf=b""
    def _l(s):
        while b"\r\n" not in s.buf: s.buf+=s.s.recv(1<<20)
        l,s.buf=s.buf.split(b"\r\n",1); return l
    def _n(s,n):
        while len(s.buf)<n+2: s.buf+=s.s.recv(1<<20)
        d=s.buf[:n]; s.buf=s.buf[n+2:]; return d
    def read(s):
        l=s._l(); t=l[:1]
        if t in (b'+',b':',b'-'): return l
        if t==b'$':
            n=int(l[1:]); return None if n<0 else s._n(n)
        if t in (b'*',b'~','%'):
            n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n*2 if t==b'%' else n)]
        return l
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a: x=x.encode() if isinstance(x,str) else (str(x).encode() if not isinstance(x,bytes) else x); o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()
    def pipe(s, cmds):
        o=b""
        for a in cmds:
            o+=b"*%d\r\n"%len(a)
            for x in a: x=x.encode() if isinstance(x,str) else (str(x).encode() if not isinstance(x,bytes) else x); o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o)
        return [s.read() for _ in cmds]

if len(sys.argv) < 4 or sys.argv[3] != "RUN":
    print("manual perf tool (mutates data); usage: bigcmd_bench.py <oracle_port> <fr_port> RUN"); sys.exit(0)
OR=int(sys.argv[1]); FRp=int(sys.argv[2])
od=R(OR); fd=R(FRp)
def setup(d):
    d.cmd("flushall")
    d.cmd("config","set","set-max-listpack-entries","128")
    d.cmd("config","set","zset-max-listpack-entries","128")
    d.cmd("config","set","hash-max-listpack-entries","128")
    # large hashtable set of 50k ints
    for base in range(0,50000,1000):
        d.cmd("sadd","bigset",*[str(base+i) for i in range(1000)])
    for base in range(0,50000,1000):
        d.cmd("sadd","bigset2",*[str(base+i+25000) for i in range(1000)])  # overlaps half
    # large zset 50k
    for base in range(0,50000,1000):
        d.cmd("zadd","bigzset",*sum([[str(base+i),"m%d"%(base+i)] for i in range(1000)],[]))
    # large list 50k
    for base in range(0,50000,5000):
        d.cmd("rpush","biglist",*["e%d"%(base+i) for i in range(5000)])
    # large hash 50k
    for base in range(0,50000,1000):
        d.cmd("hset","bighash",*sum([["f%d"%(base+i),"v%d"%(base+i)] for i in range(1000)],[]))
    # big string 1MB
    d.cmd("set","bigstr","x"*1000000)

for d in (od,fd): setup(d)

def bench(label, cmd, n):
    res={}
    for tag,d in (("redis",od),("fr",fd)):
        # warm
        d.cmd(*cmd)
        t0=time.perf_counter()
        for _ in range(n): d.cmd(*cmd)
        res[tag]=(time.perf_counter()-t0)/n*1000  # ms/op
    ratio=res["redis"]/res["fr"] if res["fr"]>0 else 0
    flag=" <<< fr SLOWER" if ratio<0.85 else (" (fr faster)" if ratio>1.15 else "")
    print(f"{label:42s} redis={res['redis']:8.3f}ms  fr={res['fr']:8.3f}ms  fr/redis={1/ratio if ratio else 0:.2f}x{flag}")

bench("SINTERSTORE bigset bigset2 (25k result)", ("sinterstore","dst","bigset","bigset2"), 30)
bench("SUNIONSTORE bigset bigset2 (75k)", ("sunionstore","dst","bigset","bigset2"), 20)
bench("SDIFFSTORE bigset bigset2 (25k)", ("sdiffstore","dst","bigset","bigset2"), 30)
bench("SINTERCARD bigset bigset2", ("sintercard","2","bigset","bigset2"), 30)
bench("SMEMBERS bigset (50k)", ("smembers","bigset"), 30)
bench("ZUNIONSTORE bigzset (50k)", ("zunionstore","zd","1","bigzset"), 30)
bench("ZRANGEBYSCORE bigzset 0 50000", ("zrangebyscore","bigzset","0","50000"), 20)
bench("ZRANGE bigzset 0 -1 (50k)", ("zrange","bigzset","0","-1"), 20)
bench("LRANGE biglist 0 -1 (50k)", ("lrange","biglist","0","-1"), 20)
bench("SORT biglist ALPHA LIMIT 0 100", ("sort","biglist","alpha","limit","0","100"), 10)
bench("SORT bigzset_as_list... HGETALL bighash (50k)", ("hgetall","bighash"), 20)
bench("LPOS biglist e49999 (tail scan)", ("lpos","biglist","e49999"), 30)
bench("GETRANGE bigstr 0 -1 (1MB)", ("getrange","bigstr","0","-1"), 50)
bench("SRANDMEMBER bigset 100", ("srandmember","bigset","100"), 50)
bench("SRANDMEMBER bigset -100 (with dup)", ("srandmember","bigset","-100"), 50)
bench("COPY bigset -> c", ("copy","bigset","c","replace"), 30)
