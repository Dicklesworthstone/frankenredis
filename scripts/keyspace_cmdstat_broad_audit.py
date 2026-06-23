#!/usr/bin/env python3
"""Broad keyspace_hits/misses + cmdstat differential audit vs redis 7.2.4.

Complements cmdstat_keyspace_parity_gate.py with a wider, option-heavy / conditional
command set (GETEX option forms, COPY, GETDEL, OBJECT subcommands, LMPOP/ZMPOP,
*STORE set-algebra, SINTERCARD, BITFIELD read+write, INCRBYFLOAT, SETRANGE) — the
class of commands where the NX/XX existence precheck mis-counted keyspace (hjk0m).
Compares aggregate keyspace_hits/misses + per-command calls/failed/rejected (usec
ignored). Both servers must have CONFIG RESETSTAT support; usage:

    python3 keyspace_cmdstat_broad_audit.py <redis_port> <fr_port>

Exit 0 + RESULT: ALL-MATCH when parity holds; exit 1 + DIVERGENCE-FOUND otherwise.
"""
import socket, sys

RP=int(sys.argv[1]) if len(sys.argv)>1 else 29522
FP=int(sys.argv[2]) if len(sys.argv)>2 else 29521

def enc(*a):
    out=b"*%d\r\n"%len(a)
    for x in a:
        b=x if isinstance(x,bytes) else str(x).encode()
        out+=b"$%d\r\n"%len(b)+b+b"\r\n"
    return out

class C:
    def __init__(s,p):
        s.s=socket.create_connection(("127.0.0.1",p)); s.buf=b""
    def _line(s):
        while b"\r\n" not in s.buf: s.buf+=s.s.recv(65536)
        l,_,s.buf=s.buf.partition(b"\r\n"); return l
    def read(s):
        l=s._line(); t=chr(l[0]); body=l[1:]
        if t in "+-:": return body
        if t=="$":
            n=int(body)
            if n<0: return None
            while len(s.buf)<n+2: s.buf+=s.s.recv(65536)
            d=s.buf[:n]; s.buf=s.buf[n+2:]; return d
        if t=="*":
            n=int(body)
            return [s.read() for _ in range(n)] if n>=0 else None
        return body
    def cmd(s,*a): s.s.sendall(enc(*a)); return s.read()

def info(c,sec):
    c.s.sendall(enc("info",sec)); r=c.read(); return r.decode("latin1") if isinstance(r,bytes) else ""

# setup + probe sequence (the SUSPECTS: conditional / option-heavy reads & writes)
SETUP=[
 ["flushall"],["set","s","12345"],["set","s2","hello"],["rpush","l","a","b","c"],
 ["sadd","st","1","2","3"],["sadd","st2","2","3","9"],["hset","h","f","v","g","w"],
 ["zadd","z","1","a","2","b"],["zadd","z2","3","b","4","c"],["set","ex","v"],["expire","ex","10000"],
]
PROBE=[
 # GETEX option forms (reads + maybe touches expiry)
 ["getex","s"],["getex","s","persist"],["getex","ex","persist"],["getex","s","ex","100"],
 ["getex","s","exat","99999999999"],["getex","s","pxat","99999999999999"],["getex","nope"],
 # COPY (reads src, writes dst)
 ["copy","s","cpy"],["copy","s","cpy","replace"],["copy","nope","cpy2"],
 # GETDEL
 ["getdel","s2"],["getdel","nope"],
 # OBJECT subcommands
 ["object","encoding","s"],["object","refcount","s"],["object","idletime","s"],["object","encoding","nope"],
 # multi-key pops
 ["lmpop","1","l","left"],["lmpop","1","nope","left"],["zmpop","1","z","min"],["zmpop","1","nope","min"],
 # store ops (read sources)
 ["sinterstore","dst","st","st2"],["sunionstore","dst2","st","st2"],["sdiffstore","dst3","st","st2"],
 ["zrangestore","zd","z","0","-1"],
 # set-algebra cardinality
 ["sintercard","2","st","st2"],["smismember","st","1","9"],["zmscore","z","a","zz"],
 # bitfield read+write
 ["bitfield","bf","set","u8","0","255"],["bitfield","bf","get","u8","0"],["bitfield","bf","incrby","u8","0","10"],
 # incrbyfloat / setrange
 ["incrbyfloat","fl","1.5"],["setrange","s","0","X"],["getrange","s","0","2"],
 # APPEND / SETRANGE on missing
 ["append","ap","x"],["setrange","sr","2","yy"],
]

def run(port):
    c=C(port)
    c.cmd("config","resetstat")
    for cmd in SETUP+PROBE:
        c.cmd(*cmd)
    st=info(c,"stats"); cs=info(c,"commandstats")
    return st,cs

def kv(st):
    h=m=0
    for ln in st.splitlines():
        if ln.startswith("keyspace_hits:"): h=int(ln.split(":")[1])
        if ln.startswith("keyspace_misses:"): m=int(ln.split(":")[1])
    return h,m

def cstats(cs):
    d={}
    for ln in cs.splitlines():
        if ln.startswith("cmdstat_"):
            name=ln[len("cmdstat_"):].split(":")[0]
            fields=dict(kv.split("=") for kv in ln.split(":")[1].split(","))
            d[name]=(fields.get("calls"),fields.get("failed_calls"),fields.get("rejected_calls"))
    return d

frst,frcs=run(FP); rdst,rdcs=run(RP)
frh,frm=kv(frst); rdh,rdm=kv(rdst)
print(f"keyspace: fr=({frh},{frm}) redis=({rdh},{rdm})  {'MATCH' if (frh,frm)==(rdh,rdm) else 'DIVERGE'}")
frc=cstats(frcs); rdc=cstats(rdcs)
allk=sorted(set(frc)|set(rdc))
bad=0
for k in allk:
    f=frc.get(k); r=rdc.get(k)
    if f!=r:
        # ignore usec; compare calls/failed/rejected only (already tuples of those)
        print(f"  cmdstat_{k}: fr={f} redis={r}  DIVERGE"); bad+=1
print(f"cmdstat divergences: {bad}")
ok=(frh,frm)==(rdh,rdm) and bad==0
print("RESULT:", "ALL-MATCH" if ok else "DIVERGENCE-FOUND")
sys.exit(0 if ok else 1)
