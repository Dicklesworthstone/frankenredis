#!/usr/bin/env python3
"""CLIENT TRACKING (RESP3) invalidation differential for CROSS-KEY writes.
A tracker reads key 'd' (registers interest), a mutator runs a cross-key write
that creates/changes 'd', and we compare the invalidation push(es) the tracker
receives — fr vs redis 7.2.4. Cross-key writes (SINTERSTORE/COPY/RENAME/MOVE/
SORT STORE/GEOSEARCHSTORE/LMOVE/RPOPLPUSH) are where invalidation gaps hide.
"""
import socket, sys, time
def conn(p):
    s=socket.create_connection(("127.0.0.1",p),timeout=5); s.settimeout(0.5); return s
class R:
    def __init__(s,p): s.s=conn(p); s.buf=b""
    def _l(s):
        while b"\r\n" not in s.buf: s.buf+=s.s.recv(1<<16)
        l,s.buf=s.buf.split(b"\r\n",1); return l
    def _n(s,n):
        while len(s.buf)<n+2: s.buf+=s.s.recv(1<<16)
        d=s.buf[:n]; s.buf=s.buf[n+2:]; return d
    def read(s):
        l=s._l(); t=l[:1]
        if t in (b'+',b':'): return l[1:].decode()
        if t==b'-': return "ERR:"+l[1:].decode()
        if t==b'$':
            n=int(l[1:]); return None if n<0 else s._n(n).decode("latin1")
        if t in (b'*',b'~','>','%'):
            n=int(l[1:]); cnt=n*2 if t==b'%' else n
            return ([t.decode()]+[s.read() for _ in range(cnt)]) if t==b'>' else [s.read() for _ in range(cnt)]
        return l.decode()
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a: x=x.encode() if isinstance(x,str) else x; o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()
    def drain_pushes(s, dur=0.3):
        evs=[]; end=time.time()+dur
        while time.time()<end:
            try: m=s.read()
            except socket.timeout: break
            except Exception: break
            if isinstance(m,list) and m and m[0]=='>': evs.append(m)
        return evs

OR=int(sys.argv[1]); FRp=int(sys.argv[2]); DIV=[]
def invset(pushes):
    keys=set()
    for p in pushes:
        # p = ['>', 'invalidate', [keys...]]  or ['>','invalidate', None] (flush)
        if len(p)>=3 and p[1]=="invalidate":
            k=p[2]
            if k is None: keys.add("__FLUSH__")
            elif isinstance(k,list): keys.update(k)
            else: keys.add(k)
    return keys

def scenario(tag, setup, mutate):
    res={}
    for port in (OR,FRp):
        ctl=R(port); ctl.cmd("flushall")
        for c in setup: ctl.cmd(*c)
        trk=R(port); trk.cmd("hello","3"); trk.cmd("client","tracking","on")
        trk.cmd("get","d")          # register interest in d
        trk.drain_pushes(0.15)      # clear any
        ctl.cmd(*mutate)            # cross-key write touching d
        time.sleep(0.05)
        res[port]=invset(trk.drain_pushes(0.35))
        trk.s.close(); ctl.s.close()
    if res[OR]!=res[FRp]:
        DIV.append(f"{tag}: mutate={mutate}  redis_inval={sorted(res[OR])}  fr_inval={sorted(res[FRp])}")

scenario("set-baseline", [], ("set","d","v"))
scenario("sinterstore-create", [("sadd","a","1","2"),("sadd","b","2","3")], ("sinterstore","d","a","b"))
scenario("sunionstore-create", [("sadd","a","1"),("sadd","b","2")], ("sunionstore","d","a","b"))
scenario("sinterstore-empty-del-existing", [("set","d","old"),("sadd","a","1"),("sadd","b","2")], ("sinterstore","d","a","b"))
scenario("copy-to-d", [("set","x","v")], ("copy","x","d"))
scenario("rename-to-d", [("set","x","v")], ("rename","x","d"))
scenario("renamenx-to-d", [("set","x","v")], ("renamenx","x","d"))
scenario("move-d-out", [("set","d","v")], ("move","d","1"))
scenario("sort-store-d", [("rpush","x","3","1","2")], ("sort","x","store","d"))
scenario("zrangestore-d", [("zadd","z","1","a","2","b")], ("zrangestore","d","z","0","-1"))
scenario("zunionstore-d", [("zadd","z1","1","a"),("zadd","z2","2","b")], ("zunionstore","d","2","z1","z2"))
scenario("geosearchstore-d", [("geoadd","g","13.36","38.11","p1"),("geoadd","g","15.08","37.5","p2")], ("geosearchstore","d","g","fromlonlat","13.36","38.11","byradius","500","km","asc"))
scenario("lmove-to-d", [("rpush","x","a","b")], ("lmove","x","d","left","right"))
scenario("rpoplpush-to-d", [("rpush","x","a","b")], ("rpoplpush","x","d"))
scenario("smove-to-d", [("sadd","x","m1"),("sadd","d","m0")], ("smove","x","d","m1"))
scenario("getdel-d", [("set","d","v")], ("getdel","d"))
scenario("setrange-d", [("set","d","hello")], ("setrange","d","0","J"))
scenario("bitop-to-d", [("set","x","abc")], ("bitop","not","d","x"))

print("="*60)
if DIV:
    for d in DIV: print("DIVERGE", d)
    print(f"FAIL — {len(DIV)} tracking-invalidation divergence(s)"); sys.exit(1)
print("PASS — cross-key CLIENT TRACKING invalidations byte-exact vs redis 7.2.4")
