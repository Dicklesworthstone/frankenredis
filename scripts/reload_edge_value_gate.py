#!/usr/bin/env python3
"""Metamorphic gate: edge-value CONTENT survives DEBUG RELOAD byte-exact (+ matches redis).

Complements reload_dump_determinism_gate (DUMP layout) and reload_encoding_survival
(encoding) by checking SEMANTIC content preservation across reload for tricky values:
special zset scores (inf/-inf/0/-0/1e100/long-precision/large-int), hash values that
are integer-looking / binary / embedded-NUL / empty, i64-boundary intset members,
binary-safe strings and list elements. Verifies (1) fr content is identical before vs
after reload, and (2) fr matches redis 7.2.4 after reload. This also bounds
frankenredis-2j9wz to LAYOUT-only (content is preserved across reload).

Usage: reload_edge_value_gate.py <oracle_port> <fr_port>   Exit 0=parity, 1=divergence.
"""
import socket, sys
def C(p): return socket.create_connection(("127.0.0.1",p),timeout=15)
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
        if t in (b'+',b':',b'-'): return l
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n)
        if t in (b'*',b'~','%'): n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n*2 if t==b'%' else n)]
        return l
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else (str(x).encode() if not isinstance(x,bytes) else x)
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()
def build(r):
    r.cmd("flushall")
    r.cmd("zadd","z","inf","pos","-inf","neg","0","zero","3.0","three","3.14159265358979","pi","1e100","big","-0","negzero","1000000000000000","intish")
    r.cmd("hset","h","a","12345","b",b"\x00\x01\x02","c","9999999999999999999","d",b"bin\xffval","e","")
    r.cmd("sadd","si","-9223372036854775808","9223372036854775807","0","-1")
    r.cmd("sadd","ss","12345",b"\x00bin","plain")
    r.cmd("set","s1","9223372036854775807"); r.cmd("set","s2","-9223372036854775808")
    r.cmd("set","s3","12345678901234567890"); r.cmd("set","sbin",b"\x00\xff\x01\xfe")
    r.cmd("rpush","l","12345","-1",b"\x00x","")
def content(r):
    return {
        'z': r.cmd("zrange","z","0","-1","WITHSCORES"),
        'h': r.cmd("hgetall","h"),
        'si': sorted(r.cmd("smembers","si")), 'ss': sorted(r.cmd("smembers","ss")),
        's1': r.cmd("get","s1"), 's2': r.cmd("get","s2"), 's3': r.cmd("get","s3"), 'sbin': r.cmd("get","sbin"),
        'l': r.cmd("lrange","l","0","-1"),
        'enc_z': r.cmd("object","encoding","z"), 'enc_h': r.cmd("object","encoding","h"),
        'enc_si': r.cmd("object","encoding","si"), 'enc_s1': r.cmd("object","encoding","s1"),
    }
def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2])); div=0
    build(od); build(fr)
    fb=content(fr); fr.cmd("debug","reload"); od.cmd("debug","reload"); fa=content(fr); oa=content(od)
    for k in fb:
        if fb[k]!=fa[k]:
            div+=1; print(f"FR-RELOAD-CHANGED {k}: before={fb[k]!r} after={fa[k]!r}")
    for k in fa:
        if fa[k]!=oa[k]:
            div+=1; print(f"FR-vs-REDIS(after reload) {k}: redis={oa[k]!r} fr={fa[k]!r}")
    print("-"*60)
    if div: print(f"FAIL — {div} edge-value reload divergence(s)"); return 1
    print("PASS — edge-value content survives reload byte-exact + matches redis 7.2.4 (bounds 2j9wz to layout-only)"); return 0
if __name__=="__main__": sys.exit(main())
