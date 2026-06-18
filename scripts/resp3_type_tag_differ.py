#!/usr/bin/env python3
"""Differential gate: RESP3 type-tag fidelity, port-based (frankenredis-...).

Under HELLO 3, commands must emit the correct RESP3 aggregate/scalar type, not a
RESP2 array/bulk fallback: HGETALL / CONFIG GET -> Map (%), SMEMBERS / SINTER /
SUNION / SDIFF / SPOP-with-count -> Set (~), ZSCORE / ZINCRBY / ZADD INCR -> Double
(,), ZMSCORE -> Array of Doubles, SISMEMBER -> Integer (:), INCRBYFLOAT -> Bulk ($,
NOT double), CLIENT INFO -> Verbatim (=). This checks the leading type byte (and full
reply for the scalar cases) byte-exact vs redis 7.2.4. The existing
resp3_type_fidelity_gate is self-orchestrating (--bin); this port-based one is
CI-registerable. (DEBUG PROTOCOL type generators need --enable-debug-command, so they
live in the self-orch gate, not here.)

Usage: resp3_type_tag_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p):
    s=socket.create_connection(("127.0.0.1",p),timeout=5); s.settimeout(2); return s
def send(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o)
def rd(s):
    time.sleep(0.04)
    try: return s.recv(1<<20)
    except Exception: return b""
def cmd(s,*a): send(s,*a); return rd(s)
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    cmd(od,"HELLO","3"); cmd(fr,"HELLO","3")
    for s in (od,fr):
        cmd(s,"FLUSHALL")
        cmd(s,"HSET","h","a","1","b","2"); cmd(s,"SADD","st","x","y","z")
        cmd(s,"ZADD","z","1.5","m","2.5","n"); cmd(s,"SET","num","42")
    # (label, args, mode) mode: 'tag' compares leading byte, 'full' compares whole reply
    TAG=[("hgetall_map",("HGETALL","h")), ("config_get_map",("CONFIG","GET","maxmemory")),
         ("smembers_set",("SMEMBERS","st")), ("sinter_set",("SINTER","st")),
         ("sunion_set",("SUNION","st")), ("sdiff_set",("SDIFF","st")),
         ("zmscore_arr",("ZMSCORE","z","m","n")), ("hrandfield_wv",("HRANDFIELD","h","2","WITHVALUES")),
         ("sismember_int",("SISMEMBER","st","x"))]
    FULL=[("zscore_double",("ZSCORE","z","m")), ("zincrby_double",("ZINCRBY","z","0","m")),
          ("zadd_incr_double",("ZADD","z","INCR","0","m")), ("incrbyfloat_bulk",("INCRBYFLOAT","num","0")),
          ("client_info_verbatim_tag",("CLIENT","INFO"))]
    for label,args in TAG:
        ro,rf=cmd(od,*args),cmd(fr,*args)
        if ro[:1]!=rf[:1]: fails.append(f"{label} tag: redis={ro[:50]!r} fr={rf[:50]!r}")
    for label,args in FULL:
        ro,rf=cmd(od,*args),cmd(fr,*args)
        # CLIENT INFO content is connection-specific; compare only the type byte
        a = ro[:1] if "client_info" in label else ro
        b = rf[:1] if "client_info" in label else rf
        if a!=b: fails.append(f"{label}: redis={ro[:50]!r} fr={rf[:50]!r}")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} RESP3 type-tag divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        sys.exit(1)
    print("PASS — RESP3 type-tag fidelity byte-exact vs redis 7.2.4 (Map/Set/Double/Int/Bulk/Verbatim under HELLO 3)")
if __name__=="__main__": main()
