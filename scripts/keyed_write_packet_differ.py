#!/usr/bin/env python3
"""Differential gate: keyed-write fast-path packets (frankenredis-...).

fr has byte-prefix fast-path packets for multi-pair keyed writes — HSET/MSET with N
field/value pairs (the bnrnp..w0i5z "N-value keyed write packet" series, N=4..16). A
fast-path must produce a reply byte-IDENTICAL to the generic handler + redis 7.2.4.
This drives HSET/MSET at every N in 4..16 (each a distinct packet) plus N=17/20 (generic
fallback) UNDER PIPELINING (the trigger condition), reading back HGETALL/MGET + HLEN +
OBJECT ENCODING, plus a deep-pipeline mixed-N batch, long-value encoding transition, and
HSET-overwrite return-count. Byte-exact vs redis 7.2.4.

Usage: keyed_write_packet_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def raw(s,b): s.sendall(b); time.sleep(0.04); return s.recv(1<<20)
def enc(*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    return o
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    raw(od,enc("FLUSHALL")); raw(fr,enc("FLUSHALL"))
    def chk(label, frames):
        ro,rf=raw(od,frames),raw(fr,frames)
        if ro!=rf: fails.append(f"{label}: redis={ro[:90]!r} fr={rf[:90]!r}")
    import re as _re
    def chk_sorted(label, frames):  # set-order-insensitive (SADD/SMEMBERS)
        ro,rf=raw(od,frames),raw(fr,frames)
        a=tuple(sorted(_re.findall(rb"\$\d+\r\n([^\r]*)\r\n", ro)))
        b=tuple(sorted(_re.findall(rb"\$\d+\r\n([^\r]*)\r\n", rf)))
        if a!=b: fails.append(f"{label}: redis={ro[:90]!r} fr={rf[:90]!r}")
    for n in range(4,21):
        args=["HSET",f"h{n}"]
        for i in range(n): args += [f"f{i}",f"val{i:03d}"]
        chk(f"hset_{n}p", enc(*args)+enc("HGETALL",f"h{n}")+enc("HLEN",f"h{n}")+enc("OBJECT","ENCODING",f"h{n}"))
    # LPUSH/RPUSH/SADD/ZADD N-value keyed-write packets (same bnrnp..w0i5z series)
    for n in range(4,17):
        chk(f"rpush_{n}p", enc("RPUSH",f"l{n}",*[f"v{i}" for i in range(n)])+enc("LRANGE",f"l{n}","0","-1")+enc("LLEN",f"l{n}"))
        chk(f"lpush_{n}p", enc("LPUSH",f"lp{n}",*[f"v{i}" for i in range(n)])+enc("LRANGE",f"lp{n}","0","-1"))
        chk_sorted(f"sadd_{n}p", enc("SADD",f"s{n}",*[f"m{i}" for i in range(n)])+enc("SMEMBERS",f"s{n}"))
        chk(f"sadd_int_{n}p", enc("SADD",f"si{n}",*[str(i) for i in range(n)])+enc("OBJECT","ENCODING",f"si{n}")+enc("SCARD",f"si{n}"))
        zargs=["ZADD",f"z{n}"]+sum([[str(i),f"m{i}"] for i in range(n)],[])
        chk(f"zadd_{n}p", enc(*zargs)+enc("ZRANGE",f"z{n}","0","-1","WITHSCORES")+enc("ZCARD",f"z{n}"))
    for n in [4,8,12,16,17,20]:
        args=["MSET"]
        for i in range(n): args += [f"mk{n}_{i}",f"mv{i}"]
        chk(f"mset_{n}p", enc(*args)+enc("MGET",*[f"mk{n}_{i}" for i in range(n)]))
    chk("hset_longval", enc("HSET","hl","f","x"*100,"g","2")+enc("OBJECT","ENCODING","hl"))
    chk("hset_overwrite", enc("HSET","ow","a","1","b","2")+enc("HSET","ow","a","9","b","8","c","7")+enc("HGETALL","ow")+enc("HLEN","ow"))
    chk("hset_odd_args", enc("HSET","bad","a","1","b"))
    batch=b""
    for n in [4,7,11,16]:
        a=["HSET",f"dp{n}"]+sum([[f"f{i}",f"v{i}"] for i in range(n)],[])
        batch+=enc(*a)
    chk("deep_mixed_N", batch+enc("DBSIZE"))
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} keyed-write-packet divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        sys.exit(1)
    print("PASS — keyed-write fast-path packets (HSET/MSET N=4..16 + fallback) byte-exact vs redis 7.2.4")
if __name__=="__main__": main()
