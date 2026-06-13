#!/usr/bin/env python3
"""Metamorphic value-encoding probe: STRING (int/embstr/raw) + LIST (listpack/
quicklist) transitions across mutating ops and DEBUG RELOAD / DUMP+RESTORE / COPY.
Asserts (TYPE, OBJECT ENCODING, DEBUG DIGEST-VALUE) match redis 7.2.4 at each step.
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
            n=int(l[1:]); return None if n<0 else s._n(n)
        if t in (b'*',b'~',b'%'):
            n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n*2 if t==b'%' else n)]
        return l.decode()
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else (str(x).encode() if not isinstance(x,bytes) else x)
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()
OR=int(sys.argv[1]); FRp=int(sys.argv[2])
od=R(OR); fd=R(FRp); DIV=[]
def norm(x):
    if isinstance(x,bytes): return x.decode("latin1")
    if isinstance(x,list): return tuple(norm(e) for e in x)
    return x
def reset():
    for d in (od,fd):
        d.cmd("flushall")
        d.cmd("config","set","list-max-listpack-size","128")
def enc(d,k): return (norm(d.cmd("type",k)), norm(d.cmd("object","encoding",k)), norm(d.cmd("debug","digest-value",k)))
def check(tag,k):
    so=enc(od,k); sf=enc(fd,k)
    if so!=sf: DIV.append(f"{tag}: key={k} redis={so} fr={sf}")
def step(tag, cmds, key):
    ro=od.cmd(*cmds[-1]) if False else None
    for c in cmds:
        a=od.cmd(*c); b=fd.cmd(*c)
        if a!=b: DIV.append(f"{tag}: reply {c} redis={a!r} fr={b!r}")
    check(tag,key)

# ---- STRING encoding transitions ----
cases = [
    ("set-int", [("set","k","12345")]),
    ("set-embstr-short", [("set","k","hello")]),
    ("set-embstr-44", [("set","k","a"*44)]),
    ("set-raw-45", [("set","k","a"*45)]),
    ("append-to-int", [("set","k","123"),("append","k","456")]),       # redis: raw (append always raws)
    ("append-makes-num", [("set","k","12"),("append","k","34")]),      # "1234" but raw (append)
    ("incr-from-int", [("set","k","10"),("incr","k")]),                # int
    ("incr-from-embstr-numeric", [("set","k","100"),("append","k",""),("incr","k")]),
    ("setrange-int", [("set","k","12345"),("setrange","k","0","9")]),  # raw
    ("getset-int", [("set","k","5"),("getset","k","7")]),              # int
    ("setbit-newkey", [("setbit","k","7","1")]),                       # raw
    ("set-incr-large", [("set","k","9999999999999"),("incr","k")]),    # int
    ("set-int-then-append-empty", [("set","k","77"),("append","k","")]),# raw (append even empty)
    ("setex-int", [("setex","k","100","42")]),                        # int
    ("set-then-getrange-nomod", [("set","k","12345")]),
]
for tag, cmds in cases:
    reset(); step("STR-"+tag, cmds, "k")
    # metamorphic: reload, dump+restore, copy preserve encoding
    od.cmd("debug","reload"); fd.cmd("debug","reload"); check("STR-"+tag+"/reload","k")
    po=od.cmd("dump","k"); pf=fd.cmd("dump","k")
    od.cmd("del","k2"); fd.cmd("del","k2")
    od.cmd("restore","k2","0",po); fd.cmd("restore","k2","0",pf); check("STR-"+tag+"/restore(k2)","k2")
    od.cmd("copy","k","k3"); fd.cmd("copy","k","k3"); check("STR-"+tag+"/copy(k3)","k3")

# ---- LIST encoding transitions (byte budget, default -2 = 8KiB and explicit count) ----
lcases = [
    ("rpush-small", [("rpush","l","a","b","c")], "128"),
    ("rpush-over-count", [("rpush","l", *[f"e{i}" for i in range(0,200)])], "128"),  # >128 -> quicklist
    ("rpush-bigval", [("rpush","l","x"*5000)], "128"),                                 # >budget -> quicklist
    ("rpush-then-lpop-shrink", [("rpush","l", *[f"e{i}" for i in range(0,200)])]+[("lpop","l") for _ in range(198)], "128"),
    ("lset-grow-bigval", [("rpush","l","a","b","c"),("lset","l","0","x"*5000)], "128"),
    ("linsert-grow", [("rpush","l","a","b"),("linsert","l","before","b","y"*5000)], "128"),
    ("rpush-default-budget", [("rpush","l", *[f"e{i}" for i in range(0,200)])], "-2"),
]
for tag, cmds, lsz in lcases:
    for d in (od,fd):
        d.cmd("flushall"); d.cmd("config","set","list-max-listpack-size",lsz)
    for c in cmds:
        a=od.cmd(*c); b=fd.cmd(*c)
    check("LIST-"+tag,"l")
    od.cmd("debug","reload"); fd.cmd("debug","reload"); check("LIST-"+tag+"/reload","l")
    po=od.cmd("dump","l"); pf=fd.cmd("dump","l")
    od.cmd("del","l2"); fd.cmd("del","l2")
    od.cmd("restore","l2","0",po); fd.cmd("restore","l2","0",pf); check("LIST-"+tag+"/restore(l2)","l2")

print("="*60)
if DIV:
    for d in DIV: print("DIVERGE", d)
    print(f"FAIL — {len(DIV)} divergence(s)"); sys.exit(1)
print("PASS — string/list encoding transitions byte-exact vs redis 7.2.4")
