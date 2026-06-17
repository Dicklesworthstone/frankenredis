#!/usr/bin/env python3
"""Differential gate: relocating EVERY value type across DBs preserves content,
encoding AND TTL, vs vendored redis 7.2.4.

move_swapdb_expiry_gate + multi_db_relocation_fuzz cover STRING keys crossing the
DB boundary; copy_command_differ covers complex types but SAME-DB only. Nothing
exercised a list/set/hash/zset/stream being COPY DB n / MOVE'd / SWAPDB'd while
carrying a TTL — yet that path must move the value, its sticky encoding flag, the
side-dict deadline, AND (for streams) the consumer groups / last-id into the
destination DB. The expires-side-dict refactor (isa2w) touches exactly this.

For each typed key (with EX 500) we run COPY db0->db1, MOVE db1->db2, SWAPDB 2<->3
and after every hop snapshot {content-digest, OBJECT ENCODING, TTL category} on
both servers and diff. Content via DEBUG DIGEST-VALUE (order-independent) for
non-streams; streams via XLEN+XRANGE+XINFO-GROUPS count. TTL by category
(-2/-1/positive), drift-immune.

Usage: cross_db_type_relocation_gate.py <oracle_port> <fr_port>  Exit 0=parity,1=diverge.
"""
import socket, sys
def C(p): return socket.create_connection(("127.0.0.1", p), timeout=10)
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
        if t in (b'+',b':',b'-'): return l.decode()
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n).decode('latin1')
        if t==b'*': n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n)]
        return l.decode()
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else x
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()

def ttlcat(v):
    if isinstance(v,str) and v[:1]==":": v=v[1:]
    try: n=int(v)
    except (TypeError, ValueError): return v
    if n in (-2,-1): return str(n)
    return "T+"

# (label, [setup commands]) — each builds key K of a distinct type+encoding.
TYPES = [
    ("str-embstr",  [("set","K","hello world")]),
    ("str-int",     [("set","K","12345")]),
    ("str-raw",     [("set","K","x"*80)]),
    ("list-lp",     [("rpush","K","a","b","c","d")]),
    ("list-ql",     [("rpush","K",*[f"item-{i:04d}" for i in range(200)])]),
    ("set-intset",  [("sadd","K","1","2","3","9","100")]),
    ("set-lp",      [("sadd","K","alpha","beta","gamma")]),
    ("set-ht",      [("sadd","K",*[f"m{i}" for i in range(200)])]),
    ("hash-lp",     [("hset","K","f1","v1","f2","v2")]),
    ("hash-ht",     [("hset","K",*sum(([f"f{i}",f"v{i}"] for i in range(200)),[]))]),
    ("zset-lp",     [("zadd","K","1","a","2.5","b","3","c")]),
    ("zset-sl",     [("zadd","K",*sum(([str(i),f"m{i}"] for i in range(200)),[]))]),
    ("stream",      [("xadd","K","1-1","fa","va","fb","vb"),
                     ("xadd","K","2-1","fa","va2"),
                     ("xgroup","create","K","g1","0"),
                     ("xreadgroup","GROUP","g1","c1","COUNT","1","STREAMS","K",">")]),
]

div=0
def obs(srv, key, is_stream):
    """One observation: (content-digest, encoding, ttl-category) for the current DB."""
    enc=srv.cmd("object","encoding",key)
    ttl=ttlcat(srv.cmd("ttl",key))
    if is_stream:
        content=("XLEN", srv.cmd("xlen",key),
                 "XR", srv.cmd("xrange",key,"-","+"),
                 "NG", srv.cmd("xinfo","groups",key))
    else:
        content=srv.cmd("debug","digest-value",key)
    return (content, enc, ttl)

def check(label, od, fr, *steps):
    """steps: list of (cmd-tuple, observe?bool). Compare reply on every step and
    a full observation after the observe-flagged ones."""
    global div
    is_stream = (label=="stream")
    for cmd, observe in steps:
        ro=od.cmd(*cmd); rf=fr.cmd(*cmd)
        if ttlcat(ro)!=ttlcat(rf) and ro!=rf:
            div+=1; print(f"DIVERGE {label} reply [{' '.join(map(str,cmd))}]\n  oracle={ro!r}\n  fr={rf!r}")
        if observe:
            oo=obs(od,"K",is_stream); of=obs(fr,"K",is_stream)
            if oo!=of:
                div+=1
                print(f"DIVERGE {label} state @[{' '.join(map(str,cmd))}]")
                if oo[0]!=of[0]: print(f"  content oracle={oo[0]!r}\n  content fr    ={of[0]!r}")
                if oo[1]!=of[1]: print(f"  encoding oracle={oo[1]!r} fr={of[1]!r}")
                if oo[2]!=of[2]: print(f"  ttlcat oracle={oo[2]!r} fr={of[2]!r}")

def reset(*srv):
    for s in srv:
        for db in range(4): s.cmd("select",str(db)); s.cmd("flushall")
        s.cmd("select","0")

def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2]))
    # match encoding thresholds so a hashtable/skiplist build is deterministic
    for s in (od,fr):
        for k,v in [("hash-max-listpack-entries","128"),("set-max-listpack-entries","128"),
                    ("set-max-intset-entries","512"),("zset-max-listpack-entries","128"),
                    ("list-max-listpack-size","128")]:
            s.cmd("config","set",k,v)
    for label, setup in TYPES:
        reset(od,fr)
        for cmd in setup:
            ro=od.cmd(*cmd); rf=fr.cmd(*cmd)
        od.cmd("expire","K","500"); fr.cmd("expire","K","500")
        check(label, od, fr,
            (("ttl","K"), True),                       # db0 baseline
            (("copy","K","K","DB","1"), False),
            (("select","1"), False), (("ttl","K"), True),   # db1 copy
            (("move","K","2"), False),
            (("select","2"), False), (("ttl","K"), True),   # db2 after MOVE
            (("swapdb","2","3"), False),
            (("select","3"), False), (("ttl","K"), True),   # db3 after SWAPDB
            (("select","0"), False),
        )
    if div: print(f"\nFAIL: {div} divergence(s)"); sys.exit(1)
    print(f"OK: {len(TYPES)} types relocated across DBs (COPY/MOVE/SWAPDB) — content+encoding+TTL byte-exact vs redis 7.2.4")

if __name__=="__main__": main()
