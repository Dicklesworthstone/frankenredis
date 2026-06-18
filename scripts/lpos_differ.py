#!/usr/bin/env python3
"""Differential gate for LPOS (key element [RANK r] [COUNT c] [MAXLEN m]).

LPOS has rich, under-tested option semantics with no dedicated differ: RANK
(1-based, negative = search from tail, 0 = error), COUNT (0 = all matches,
negative = error), MAXLEN (scan-length cap, negative = error), and their
combinations + RANK overshoot. Diffs every reply vs vendored redis 7.2.4.

Usage: lpos_differ.py <oracle_port> <fr_port>   Exit 0=parity, 1=divergence.
"""
import socket, sys
def C(p): return socket.create_connection(("127.0.0.1",p),timeout=10)
class R:
    def __init__(s,p): s.s=C(p); s.buf=b""
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
        if t in (b'*',b'~'): n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n)]
        return l.decode()
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else x
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()
CASES=[
 ("basic-a",["lpos","l","a"]), ("rank2",["lpos","l","a","RANK","2"]),
 ("rank-neg1",["lpos","l","a","RANK","-1"]), ("rank-neg2",["lpos","l","a","RANK","-2"]),
 ("count0-all",["lpos","l","a","COUNT","0"]), ("count2",["lpos","l","a","COUNT","2"]),
 ("rank-neg1-count0",["lpos","l","a","RANK","-1","COUNT","0"]),
 ("rank2-count0",["lpos","l","a","RANK","2","COUNT","0"]),
 ("maxlen3",["lpos","l","a","COUNT","0","MAXLEN","3"]),
 ("maxlen-rankneg",["lpos","l","a","RANK","-1","COUNT","0","MAXLEN","2"]),
 ("missing-elem",["lpos","l","z"]), ("missing-elem-count",["lpos","l","z","COUNT","0"]),
 ("missing-key",["lpos","nope","a"]), ("rank0-err",["lpos","l","a","RANK","0"]),
 ("count-neg-err",["lpos","l","a","COUNT","-1"]), ("maxlen-neg-err",["lpos","l","a","MAXLEN","-1"]),
 ("rank-big",["lpos","l","a","RANK","100"]), ("rank-negbig",["lpos","l","a","RANK","-100"]),
]
def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2])); div=0
    def cleanup():
        for s in (od,fr):
            try:
                s.cmd("flushall")
            except Exception:
                pass
    for label,c in CASES:
        for s in (od,fr): s.cmd("flushall"); s.cmd("rpush","l","a","b","c","a","b","c","a")
        ro=od.cmd(*c); rf=fr.cmd(*c)
        if ro!=rf: div+=1; print(f"DIVERGE {label} [{' '.join(c)}]\n  oracle: {ro}\n  fr    : {rf}")
    # wrongtype (separate setup)
    for s in (od,fr): s.cmd("flushall"); s.cmd("set","s","x")
    ro=od.cmd("lpos","s","a"); rf=fr.cmd("lpos","s","a")
    if ro!=rf: div+=1; print(f"DIVERGE wrongtype\n  oracle:{ro}\n  fr:{rf}")
    print("-"*60)
    cleanup()
    if div: print(f"FAIL — {div} divergence(s)"); return 1
    print(f"PASS — LPOS byte-exact vs redis 7.2.4 ({len(CASES)+1} cases)"); return 0
if __name__=="__main__": sys.exit(main())
