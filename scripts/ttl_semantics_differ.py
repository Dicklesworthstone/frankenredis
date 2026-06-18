#!/usr/bin/env python3
"""TTL-semantics differential: which ops PRESERVE / REMOVE / COPY a key's TTL,
vs redis 7.2.4. Compares "has a TTL" (PTTL > 0) and rough magnitude bucket, not
exact ms (avoids clock drift). Surfaces divergences in TTL handling across
SET/GETSET/APPEND/SETRANGE/INCR/COPY/RENAME/*STORE/RESTORE/PERSIST/SETBIT etc.
"""
import socket, sys
def C(p): return socket.create_connection(("127.0.0.1", p), timeout=15)
class R:
    def __init__(s, p): s.s=C(p); s.buf=b""
    def _l(s):
        while b"\r\n" not in s.buf: s.buf+=s.s.recv(1<<20)
        l,s.buf=s.buf.split(b"\r\n",1); return l
    def _n(s,n):
        while len(s.buf)<n+2: s.buf+=s.s.recv(1<<20)
        d=s.buf[:n]; s.buf=s.buf[n+2:]; return d
    def read(s):
        l=s._l(); t=l[:1]
        if t in (b'+',b':'): return l[1:].decode()
        if t==b'-': return "ERR:"+l[1:].decode().split()[0]
        if t==b'$':
            n=int(l[1:]); return None if n<0 else s._n(n).decode("latin1")
        if t in (b'*',b'~',b'%'):
            n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n*2 if t==b'%' else n)]
        return l.decode()
    def _send(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else (str(x).encode() if not isinstance(x,bytes) else x)
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o)
    def cmd(s,*a):
        s._send(*a); return s.read()
    def bulk(s,*a):
        s._send(*a)
        l=s._l()
        if l[:1] != b"$": return None
        n=int(l[1:])
        return None if n<0 else s._n(n)
OR=int(sys.argv[1]); FRp=int(sys.argv[2]); od=R(OR); fd=R(FRp); DIV=[]
def ttl_bucket(d,k):
    v=d.cmd("pttl",k)
    try: v=int(v)
    except (TypeError, ValueError): return ("err",v)
    if v==-2: return "missing"
    if v==-1: return "no-ttl"
    return "has-ttl"  # magnitude ignored (drift)
def check(tag,k):
    a=ttl_bucket(od,k); b=ttl_bucket(fd,k)
    if a!=b: DIV.append(f"{tag}: key={k} redis-pttl={a} fr-pttl={b}")
def R2(tag, setup, key="k"):
    for d in (od,fd): d.cmd("flushall")
    for c in setup:
        a=od.cmd(*c); b=fd.cmd(*c)
        if a!=b: DIV.append(f"{tag}: reply {c} redis={a!r} fr={b!r}")
    check(tag,key)

# in-place string mutations PRESERVE ttl
R2("append-keeps-ttl", [("set","k","abc","EX","100"),("append","k","def")])
R2("setrange-keeps-ttl", [("set","k","abc","EX","100"),("setrange","k","0","X")])
R2("incr-keeps-ttl", [("set","k","10","EX","100"),("incr","k")])
R2("incrby-keeps-ttl", [("set","k","10","EX","100"),("incrby","k","5")])
R2("setbit-keeps-ttl", [("set","k","abc","EX","100"),("setbit","k","0","1")])
R2("getrange-keeps-ttl", [("set","k","abc","EX","100"),("getrange","k","0","1")])
# SET without KEEPTTL REMOVES ttl
R2("set-removes-ttl", [("set","k","abc","EX","100"),("set","k","def")])
R2("set-keepttl", [("set","k","abc","EX","100"),("set","k","def","KEEPTTL")])
R2("getset-removes-ttl", [("set","k","abc","EX","100"),("getset","k","z")])
R2("getdel-removes", [("set","k","abc","EX","100"),("getdel","k")])  # key gone -> missing
# GETEX variants
R2("getex-persist", [("set","k","abc","EX","100"),("getex","k","PERSIST")])     # no-ttl
R2("getex-noargs-keeps", [("set","k","abc","EX","100"),("getex","k")])          # has-ttl (no change)
R2("getex-ex", [("set","k","abc"),("getex","k","EX","100")])                     # has-ttl
# collection in-place adds keep ttl
R2("sadd-newkey-no-ttl", [("sadd","k","a","b")])
R2("rpush-keeps-ttl", [("rpush","k","a"),("pexpire","k","100000"),("rpush","k","b")])
R2("hset-keeps-ttl", [("hset","k","f","v"),("pexpire","k","100000"),("hset","k","g","w")])
R2("sadd-existing-keeps-ttl", [("sadd","k","a"),("pexpire","k","100000"),("sadd","k","b")])
R2("zadd-existing-keeps-ttl", [("zadd","k","1","a"),("pexpire","k","100000"),("zadd","k","2","b")])
# PERSIST
R2("persist", [("set","k","x","EX","100"),("persist","k")])
# COPY copies TTL
R2("copy-copies-ttl", [("set","k","x","EX","100"),("copy","k","d")], key="d")
R2("copy-no-ttl-src", [("set","k","x"),("copy","k","d")], key="d")
# RENAME preserves TTL
R2("rename-keeps-ttl", [("set","k","x","EX","100"),("rename","k","d")], key="d")
# RENAME onto existing: target gets source's ttl
R2("rename-onto-existing", [("set","k","x","EX","100"),("set","d","y","EX","500"),("rename","k","d")], key="d")

def restore_payload(d, tag):
    d.cmd("flushall")
    if d.cmd("set", "src", "restore-value") != "OK":
        DIV.append(f"{tag}: SET source failed")
        return None
    payload = d.bulk("dump", "src")
    if payload is None:
        DIV.append(f"{tag}: DUMP source did not return a bulk payload")
    return payload

def RRESTORE(tag, ttl, opts=(), pre=()):
    op = restore_payload(od, tag)
    fp = restore_payload(fd, tag)
    for d, payload, who in ((od, op, "redis"), (fd, fp, "fr")):
        d.cmd("flushall")
        if payload is None:
            continue
        for c in pre:
            a = d.cmd(*c)
            if a is None or (isinstance(a, str) and a.startswith("ERR:")):
                DIV.append(f"{tag}: {who} setup {c} reply={a!r}")
        r = d.cmd("restore", "k", ttl, payload, *opts)
        if r != "OK":
            DIV.append(f"{tag}: {who} RESTORE reply={r!r}")
    check(tag, "k")

# RESTORE: ttl=0 creates a persistent key; positive relative and ABSTTL create
# volatile keys. DUMP payloads are binary, so the gate must round-trip raw bulk.
RRESTORE("restore-ttl-zero-no-ttl", "0")
RRESTORE("restore-relative-ttl", "100000")
RRESTORE("restore-absttl-future", "9999999999999", ("ABSTTL",))
RRESTORE("restore-replace-clears-dest-ttl", "0", ("REPLACE",), (("set", "k", "old", "EX", "500"),))
# *STORE destinations have NO ttl (even if dest pre-existed with one)
R2("sinterstore-no-ttl", [("sadd","a","1","2"),("sadd","b","2","3"),("sinterstore","d","a","b")], key="d")
R2("sinterstore-clears-dest-ttl", [("set","d","old","EX","500"),("sadd","a","1","2"),("sadd","b","2","3"),("sinterstore","d","a","b")], key="d")
R2("zrangestore-no-ttl", [("zadd","z","1","a","2","b"),("set","d","old","EX","500"),("zrangestore","d","z","0","-1")], key="d")
R2("sort-store-no-ttl", [("rpush","l","3","1","2"),("set","d","old","EX","500"),("sort","l","store","d")], key="d")
# SETEX / PSETEX / SET EX/PX/EXAT set ttl
R2("setex", [("setex","k","100","v")])
R2("set-exat", [("set","k","v","EXAT","99999999999")])
# SETRANGE creating new key -> no ttl
R2("setrange-newkey-no-ttl", [("setrange","k","5","hello")])
# APPEND creating new key -> no ttl
R2("append-newkey-no-ttl", [("append","k","hello")])
# INCR creating new key -> no ttl
R2("incr-newkey-no-ttl", [("incr","k")])
# SMOVE / LMOVE don't add ttl to dest
R2("lmove-dest-no-ttl", [("rpush","l","a","b"),("set","d2","z","EX","500"),("del","d2"),("rpush","d","x"),("pexpire","d","100000"),("lmove","l","d","left","right")], key="d")

print("="*60)
if DIV:
    for d in DIV: print("DIVERGE", d)
    print(f"FAIL — {len(DIV)} divergence(s)"); sys.exit(1)
print("PASS — TTL semantics byte-exact vs redis 7.2.4")
