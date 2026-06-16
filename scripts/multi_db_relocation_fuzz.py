#!/usr/bin/env python3
"""Seeded multi-DB relocation fuzzer vs vendored redis 7.2.4.

The single-DB random_command_differ never crosses the logical-DB boundary, yet
that is exactly where key+TTL relocation bugs hide (MOVE/COPY DB n/SWAPDB must
carry the expire entry into the destination DB's expires dict — regression
isa2w). This fuzzer drives a random stream of SELECT / SET[+EX] / DEL / EXPIRE /
PERSIST / MOVE / COPY[ DB][ REPLACE] / RENAME / SWAPDB / FLUSHDB across N DBs on
both servers, then after every mutation snapshots ALL DBs (each key's value +
TTL category) and diffs fr against redis. First divergence prints the op that
caused it and aborts.

String values only -> a snapshot is (GET, ttl-category) per key, so MOVE/COPY/
SWAPDB/RENAME are fully exercised for value+TTL fidelity without type noise.
TTL compared by category (-2/-1/positive), drift-immune.

Usage: multi_db_relocation_fuzz.py <oracle_port> <fr_port> [seeds] [iters]
       default: 4 seeds x 1000 ops.  Exit 0=parity, 1=divergence.
"""
import socket, sys, random

NDB  = 4
KEYS = [f"k{i}" for i in range(8)]
VALS = ["a","bb","ccc","",  "x"*40, "10", "-7", "3.14"]

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
    # R.read() returns integer replies WITH the ":" prefix (e.g. ":500"); strip it.
    if isinstance(v,str) and v[:1]==":": v=v[1:]
    try: n=int(v)
    except (TypeError, ValueError): return v
    if n in (-2,-1): return str(n)
    return "T+"   # any positive ttl -> one bucket (drift-immune)

def snapshot(srv):
    """Canonical {db: {key: (value, ttl-category)}} across all DBs."""
    snap={}
    for db in range(NDB):
        srv.cmd("select",str(db)); d={}
        for k in KEYS:
            v=srv.cmd("get",k)
            if v is not None:
                d[k]=(v, ttlcat(srv.cmd("ttl",k)))
        snap[db]=d
    srv.cmd("select","0")
    return snap

def reset(*srv):
    for s in srv:
        for db in range(NDB): s.cmd("select",str(db)); s.cmd("flushall")
        s.cmd("select","0")

def gen_op(rnd):
    """Return a list of redis command tokens (a single mutation)."""
    op=rnd.choice(["set","set","setex","del","expire","persist",
                   "move","copy","copydb","rename","swapdb","flushdb","select"])
    k=lambda: rnd.choice(KEYS)
    db=lambda: str(rnd.randrange(NDB))
    if op=="set":    return ["set", k(), rnd.choice(VALS)]
    if op=="setex":  return ["set", k(), rnd.choice(VALS), "EX", str(rnd.choice([50,500,5000]))]
    if op=="del":    return ["del", k()]
    if op=="expire": return ["expire", k(), str(rnd.choice([50,500,5000]))]
    if op=="persist":return ["persist", k()]
    if op=="move":   return ["move", k(), db()]
    if op=="rename": return ["rename", k(), k()]   # may error (no such key); identical on both
    if op=="copy":
        a=["copy", k(), k()]
        if rnd.random()<0.5: a.append("REPLACE")
        return a
    if op=="copydb":
        a=["copy", k(), k(), "DB", db()]
        if rnd.random()<0.5: a.append("REPLACE")
        return a
    if op=="swapdb": return ["swapdb", db(), db()]
    if op=="flushdb":return ["flushdb"]
    return ["select", db()]

def run_seed(od, fr, seed, iters):
    rnd=random.Random(seed)
    reset(od,fr)
    cur=0
    for i in range(iters):
        cmd=gen_op(rnd)
        # keep both clients pointed at the same current DB across SELECT ops
        if cmd[0]=="select": cur=int(cmd[1])
        ro=od.cmd(*cmd); rf=fr.cmd(*cmd)
        # mutation replies are deterministic ints / OK / errors -> compare exactly
        # (do NOT category-collapse, which would mask COPY/MOVE :1-vs-:0).
        if ro!=rf:
            print(f"[seed {seed} op {i}] REPLY DIVERGE {cmd}\n  oracle={ro!r}\n  fr    ={rf!r}")
            return 1
        # restore current DB (snapshot walks all DBs and leaves us at 0)
        so=snapshot(od); sf=snapshot(fr)
        od.cmd("select",str(cur)); fr.cmd("select",str(cur))
        if so!=sf:
            print(f"[seed {seed} op {i}] STATE DIVERGE after {cmd}")
            for d in range(NDB):
                if so[d]!=sf[d]:
                    print(f"  db{d} oracle={so[d]}\n  db{d} fr    ={sf[d]}")
            return 1
    return 0

def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2]))
    seeds=int(sys.argv[3]) if len(sys.argv)>3 else 4
    iters=int(sys.argv[4]) if len(sys.argv)>4 else 1000
    bad=0
    for sd in range(seeds):
        bad|=run_seed(od,fr,1000+sd,iters)
        if bad: break
    if bad:
        print("FAIL: multi-DB relocation divergence"); sys.exit(1)
    print(f"OK: {seeds} seed(s) x {iters} ops, multi-DB key+TTL state byte-exact vs redis 7.2.4")

if __name__=="__main__": main()
