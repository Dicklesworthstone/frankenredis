#!/usr/bin/env python3
"""Differential gate: INFO keyspace `keys`/`expires` counts stay exact across
TTL-mutating operations, vs vendored redis 7.2.4.

The volatile-only expiry side-dict refactor (uhthd / eeb3c8d01) moved per-key TTLs
into a separate db->expires-shaped map. Its bookkeeping (db_key_counts /
db_expires_counts) drives INFO keyspace `db<N>:keys=K,expires=E`. A miscount on any
TTL transition — overwrite clearing a TTL, GETSET, DEL of a volatile key, RENAME /
COPY / MOVE carrying (or not) a deadline, lazy expiry, re-EXPIRE — would silently
skew `expires`. The random reply-fuzzers never read INFO keyspace, so this is
uncovered. This gate drives every such transition across db0/db1/db2 and asserts the
keys+expires counts match redis after each.

`avg_ttl` is EXCLUDED — it is redis's sampled/decaying rolling estimate (frankenredis
-xn7xr), inherently non-byte-matchable; fr reports 0.

Usage: keyspace_expires_count_gate.py <oracle_port> <fr_port>  Exit 0=parity,1=diverge.
"""
import socket, sys, time
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
        if t in (b'+',b':',b'-'): return l.decode('latin1')
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n).decode('latin1')
        if t==b'*': n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n)]
        return l.decode('latin1')
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else x
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()

def keyspace_counts(srv):
    """{db: (keys, expires)} from INFO keyspace, ignoring avg_ttl."""
    info = srv.cmd("info", "keyspace"); out={}
    for line in info.split("\r\n"):
        if line.startswith("db") and ":" in line:
            db = line.split(":",1)[0]
            fields = dict(kv.split("=") for kv in line.split(":",1)[1].split(","))
            out[db] = (int(fields.get("keys",0)), int(fields.get("expires",0)))
    return out

div=0
def main():
    global div
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2]))
    def reset():
        for s in (od,fr):
            for db in range(3): s.cmd("select",str(db)); s.cmd("flushall")
            s.cmd("select","0")
    def step(label, *ops):
        global div
        for s in (od,fr):
            for op in ops: s.cmd(*op)
        a, b = keyspace_counts(od), keyspace_counts(fr)
        if a!=b:
            div+=1; print(f"DIVERGE {label}\n  oracle={a}\n  fr    ={b}")

    reset()
    step("4 keys 2 volatile", ("mset","a","1","b","2","c","3","d","4"),
         ("expire","a","5000"), ("expire","b","5000"))
    step("persist clears one", ("persist","a"))
    step("overwrite clears ttl", ("set","b","new"))
    step("set EX adds ttl", ("set","c","v","EX","1000"))
    step("getset clears ttl", ("getset","c","v2"))
    step("del volatile", ("set","e","v","EX","1000"), ("del","e"))
    step("rename volatile", ("set","f","v","EX","1000"), ("rename","f","g"))
    step("renamenx volatile onto new", ("set","h","v","EX","1000"), ("renamenx","h","h2"))
    step("copy volatile", ("set","i","v","EX","1000"), ("copy","i","i2"))
    step("copy strips? no - copy carries ttl", ("set","j","v","EX","1000"), ("copy","j","j2","REPLACE"))
    step("re-expire volatile", ("set","k","v","EX","1000"), ("expire","k","2000"))
    step("pexpire then persist", ("set","l","v","PX","900000"), ("persist","l"))
    step("expireat past deletes", ("set","m","v","EX","1000"), ("expireat","m","1"))
    step("set KEEPTTL keeps", ("set","n","v","EX","1000"), ("set","n","v2","KEEPTTL"))
    # cross-DB: MOVE / SWAPDB volatile
    reset()
    step("move volatile to db1", ("set","mv","v","EX","1000"), ("move","mv","1"))
    step("copy volatile to db2", ("set","cp","v","EX","1000"), ("copy","cp","cp","DB","2"))
    step("swapdb 0<->1", ("swapdb","0","1"))
    # lazy expiry
    reset()
    for s in (od,fr): s.cmd("set","x","v","PX","1"); s.cmd("set","y","v")
    time.sleep(0.15)
    for s in (od,fr): s.cmd("get","x")  # force lazy expire
    a,b = keyspace_counts(od), keyspace_counts(fr)
    if a!=b: div+=1; print(f"DIVERGE lazy-expire\n  oracle={a}\n  fr={b}")

    if div: print(f"\nFAIL: {div} divergence(s)"); sys.exit(1)
    print("OK: INFO keyspace keys/expires counts byte-exact across TTL transitions vs redis 7.2.4")

if __name__=="__main__": main()
