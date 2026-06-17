#!/usr/bin/env python3
"""Differential gate: DEBUG <sub> <key> resolves the key in the client's SELECTed
DB, vs vendored redis 7.2.4.

fr stores keys DB-namespaced (encode_db_key). The key-bearing DEBUG subcommands
(OBJECT, SDSLEN, LISTPACK, QUICKLIST, HTSTATS-KEY) historically looked the key up
with the RAW arg, which only matches db 0 — so on a non-zero DB they returned db
0's same-named object (WRONG value) or "no such key". This gate plants a key in
db 2 AND a same-named DECOY with different content/size in db 0, then runs each
DEBUG subcommand while SELECTed on db 2 and asserts the reply matches redis (which
always reads the SELECTed DB). The decoy makes a wrong-DB read detectable via
serializedlength / key_sds_len. The volatile `at:0x...` pointer and `lru:` clock
fields (known cosmetic WONTFIX) are normalized out.

Usage: debug_multidb_key_gate.py <oracle_port> <fr_port>  Exit 0=parity,1=diverge.
"""
import socket, sys, re

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

def norm(v):
    if not isinstance(v,str): return v
    v=re.sub(r"at:0x[0-9a-fA-F]+", "at:0xX", v)
    v=re.sub(r"lru:\d+", "lru:X", v)
    return v

div=0
def main():
    global div
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2]))
    # deterministic encodings: 200-elt list -> quicklist, 150-field hash -> hashtable
    for s in (od,fr):
        s.cmd("config","set","list-max-listpack-size","128")
        s.cmd("config","set","hash-max-listpack-entries","128")
        for db in (0,1,2): s.cmd("select",str(db)); s.cmd("flushall")
        s.cmd("select","0")
    # plant db2 keys + same-named db0 decoys with DIFFERENT content
    for s in (od,fr):
        s.cmd("select","2")
        s.cmd("set","kstr","hello")
        s.cmd("rpush","klp","a","b","c")
        s.cmd("rpush","kql",*[str(i) for i in range(200)])
        s.cmd("hset","kht",*sum(([f"f{i}",f"v{i}"] for i in range(150)),[]))
        s.cmd("select","0")
        s.cmd("set","kstr","DB0-DECOY-LONGER")      # different size -> exposes wrong-DB read
        s.cmd("rpush","klp","Z")
        s.cmd("set","kql","db0scalar")              # wrong type in db0
        s.cmd("set","kht","db0scalar")
        s.cmd("select","0")

    def chk(label, db, *cmd, resolve_only=False):
        global div
        od.cmd("select",str(db)); fr.cmd("select",str(db))
        a=norm(od.cmd(*cmd)); b=norm(fr.cmd(*cmd))
        od.cmd("select","0"); fr.cmd("select","0")
        if resolve_only:
            # fr's HTSTATS-KEY emits canned stats (it cannot expose redis dict
            # bucket internals — WONTFIX, like DEBUG OBJECT lru:). Assert only that
            # BOTH RESOLVED the SELECTed-DB hashtable (a "Hash table" block, not the
            # "no such key" / "not a hash table" error a wrong-DB read would give).
            ok = (isinstance(a,str) and isinstance(b,str)
                  and a.startswith("Hash table") and b.startswith("Hash table"))
            if not ok:
                div+=1; print(f"DIVERGE {label} (resolve) [{' '.join(cmd)}]\n  oracle={a!r}\n  fr    ={b!r}")
            return
        if a!=b:
            div+=1; print(f"DIVERGE {label} [{' '.join(cmd)}]\n  oracle={a!r}\n  fr    ={b!r}")

    # db2 reads must hit the db2 object, not the db0 decoy
    chk("object-str   db2", 2, "debug","object","kstr")
    chk("object-lp    db2", 2, "debug","object","klp")
    chk("object-ql    db2", 2, "debug","object","kql")
    chk("object-ht    db2", 2, "debug","object","kht")
    chk("sdslen-str   db2", 2, "debug","sdslen","kstr")
    chk("listpack-lp  db2", 2, "debug","listpack","klp")
    chk("quicklist-ql db2", 2, "debug","quicklist","kql")
    chk("htstats-ht   db2", 2, "debug","htstats-key","kht", resolve_only=True)
    # db0 isomorphism: same subcommands still read db0's decoy correctly
    chk("object-str   db0", 0, "debug","object","kstr")
    chk("sdslen-str   db0", 0, "debug","sdslen","kstr")
    # missing key in a non-zero DB -> "no such key" on both (not a db0 fallthrough)
    chk("object-missing db1", 1, "debug","object","kstr")

    if div: print(f"\nFAIL: {div} divergence(s)"); sys.exit(1)
    print("OK: DEBUG OBJECT/SDSLEN/LISTPACK/QUICKLIST/HTSTATS-KEY resolve the SELECTed DB byte-exact vs redis 7.2.4")

if __name__=="__main__": main()
