#!/usr/bin/env python3
"""Destination-encoding gate for *STORE / cross-key set-algebra commands.

Redis creates the destination of SINTERSTORE/SUNIONSTORE/SDIFFSTORE/ZRANGESTORE/
ZUNION|INTER|DIFFSTORE/GEOSEARCHSTORE/COPY/SORT...STORE/SMOVE/LMOVE/RPOPLPUSH with
its encoding RE-DERIVED from the result content under the current config (intset/
listpack/hashtable, listpack/skiplist, listpack/quicklist). This probes whether
fr's destination encoding (+ digest + type) matches redis 7.2.4 across encodings
and threshold boundaries.
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
        if t==b'-': return "ERR:"+l[1:].decode()
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
od=R(OR); fd=R(FRp)
DIV=[]
# Open frankenredis-v4ba8 set facet: set-algebra rebuild of an ALL-INT result
# whose cardinality overflows set-max-intset-entries must become hashtable
# (redis builds incrementally -> intset overflow -> HT), but fr's bulk
# set_value_entry re-derives via set_fits_* -> listpack. PATH-DEPENDENT: one-shot
# SADD of the same content is listpack in redis too, so the fix must live in the
# rebuild path, not the shared refresh. Tracked so this gate fails only on a NEW
# (esp. zset) regression and flags when the set facet is fixed.
KNOWN_V4BA8 = {"sunionstore-intset-overflow"}
def norm(x):
    if isinstance(x,bytes): return x.decode("latin1")
    if isinstance(x,list): return tuple(norm(e) for e in x)
    return x
def reset(cfg=None):
    for d in (od,fd):
        d.cmd("flushall")
        base={"hash-max-listpack-entries":128,"hash-max-listpack-value":64,
              "set-max-listpack-entries":128,"set-max-intset-entries":512,
              "set-max-listpack-value":64,"zset-max-listpack-entries":128,
              "zset-max-listpack-value":64,"list-max-listpack-size":128}
        if cfg: base.update(cfg)
        for k,v in base.items(): d.cmd("config","set",k,str(v))
def st(d,key):
    return (norm(d.cmd("type",key)), norm(d.cmd("object","encoding",key)), norm(d.cmd("debug","digest-value",key)))
def check(tag,key):
    so=st(od,key); sf=st(fd,key)
    if so!=sf: DIV.append(f"{tag}: key={key} redis={so} fr={sf}")
def run(tag, setup, store_cmd, dest, cfg=None):
    reset(cfg)
    for d in (od,fd):
        for c in setup: d.cmd(*c)
    ro=od.cmd(*store_cmd); rf=fd.cmd(*store_cmd)
    if ro!=rf: DIV.append(f"{tag}: STORE reply redis={ro!r} fr={rf!r}")
    check(tag, dest)

# --- SINTERSTORE / SUNIONSTORE / SDIFFSTORE: int sets -> intset dest? str -> listpack? big -> hashtable ---
run("sinterstore-int", [("sadd","a","1","2","3","4"),("sadd","b","2","3","4","5")], ("sinterstore","d","a","b"),"d")
run("sunionstore-int", [("sadd","a","1","2","3"),("sadd","b","4","5","6")], ("sunionstore","d","a","b"),"d")
run("sdiffstore-str", [("sadd","a","x","y","z"),("sadd","b","y")], ("sdiffstore","d","a","b"),"d")
# union of int sets producing >set-max-intset-entries members -> hashtable (intset overflow on store)
run("sunionstore-intset-overflow",
    [("sadd","a",*[str(i) for i in range(0,6)]),("sadd","b",*[str(i) for i in range(6,12)])],
    ("sunionstore","d","a","b"),"d", cfg={"set-max-intset-entries":4,"set-max-listpack-entries":128})
# union producing >listpack-entries strings -> hashtable
run("sunionstore-listpack-overflow",
    [("sadd","a",*[f"s{i}" for i in range(0,5)]),("sadd","b",*[f"t{i}" for i in range(0,5)])],
    ("sunionstore","d","a","b"),"d", cfg={"set-max-listpack-entries":4})
# union DOWN: a is hashtable (big member) but result is small ints -> intset?
run("sinterstore-down-from-hashtable",
    [("sadd","a","1","2","3", "x"*80),("sadd","b","1","2","3")],
    ("sinterstore","d","a","b"),"d")

# --- ZRANGESTORE / ZUNIONSTORE / ZINTERSTORE / ZDIFFSTORE ---
run("zrangestore", [("zadd","z","1","a","2","b","3","c")], ("zrangestore","d","z","0","-1"),"d")
run("zunionstore", [("zadd","z1","1","a","2","b"),("zadd","z2","3","c")], ("zunionstore","d","2","z1","z2"),"d")
run("zinterstore", [("zadd","z1","1","a","2","b"),("zadd","z2","5","b")], ("zinterstore","d","2","z1","z2"),"d")
run("zdiffstore", [("zadd","z1","1","a","2","b","3","c"),("zadd","z2","1","a")], ("zdiffstore","d","2","z1","z2"),"d")
# zunionstore producing > zset-max-listpack-entries -> skiplist
run("zunionstore-skiplist-overflow",
    [("zadd","z1",*sum([[str(i),f"m{i}"] for i in range(0,5)],[])),
     ("zadd","z2",*sum([[str(i),f"n{i}"] for i in range(0,5)],[]))],
    ("zunionstore","d","2","z1","z2"),"d", cfg={"zset-max-listpack-entries":4})
# zrangestore of an over-threshold source built under higher cfg -> after cfg lower? (store re-derives)
run("zrangestore-big-value",
    [("zadd","z","1", "x"*80)],
    ("zrangestore","d","z","0","-1"),"d")

# --- COPY preserves encoding ---
run("copy-intset", [("sadd","a","1","2","3")], ("copy","a","d"),"d")
run("copy-hashtable", [("sadd","a","1","2","3","x"*80)], ("copy","a","d"),"d")

# --- SORT ... STORE produces a list (quicklist/listpack) ---
run("sort-store", [("rpush","l","3","1","2")], ("sort","l","store","d"),"d")
run("sort-store-big",
    [("rpush","l", *[f"{i}" for i in range(0,5)])],
    ("sort","l","store","d"),"d", cfg={"list-max-listpack-size":4})

# --- SMOVE / LMOVE / RPOPLPUSH dest encoding ---
run("lmove-newdest", [("rpush","l","a","b","c")], ("lmove","l","d","left","right"),"d")
run("rpoplpush-newdest", [("rpush","l","a","b","c")], ("rpoplpush","l","d"),"d")

def tag_of(line): return line.split(":",1)[0]
new = [d for d in DIV if tag_of(d) not in KNOWN_V4BA8]
known = [d for d in DIV if tag_of(d) in KNOWN_V4BA8]
for d in known: print("KNOWN-v4ba8", d)
for d in new: print("DIVERGE", d)
seen = {tag_of(d) for d in DIV}
fixed = [t for t in KNOWN_V4BA8 if t not in seen]
if fixed: print(f"NOTE: v4ba8 set facet appears FIXED for {fixed} — drop from KNOWN_V4BA8")
print("="*60)
if new:
    print(f"FAIL — {len(new)} NEW *STORE destination-encoding divergence(s)"); sys.exit(1)
print(f"PASS — *STORE/cross-key destination encoding byte-exact vs redis 7.2.4 ({len(known)} known-v4ba8 tracked)")
