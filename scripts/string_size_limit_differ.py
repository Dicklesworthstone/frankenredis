#!/usr/bin/env python3
"""Regression gate: SETRANGE/APPEND/SETBIT proto-max-bulk-len boundary (frankenredis-vkl1p).

Unlike BITFIELD (which redis bounds by offset byte-index — see g3ioa), SET/APPEND/
SETRANGE are bounded by the RESULTING STRING LENGTH against proto-max-bulk-len (a
result of exactly 512 MiB is allowed, one byte more errors). SETBIT is bounded by
its bit offset (max bit 4294967295 -> 512 MiB string OK; >= 2^32 errors "bit
offset...out of range"). fr is byte-exact at the DEFAULT 512 MiB config; this pins
those boundaries so the g3ioa BITFIELD fix isn't mis-applied to length-checked cmds.

KNOWN, NOT asserted here (frankenredis-uwhyl): fr HARDCODES 512 MiB in all these size
checks instead of the proto-max-bulk-len config, so a non-default (e.g. 1 MiB)
proto-max-bulk-len makes fr accept oversized writes redis rejects — an architectural
config-plumbing fix. This gate runs at the default config, where fr is correct.

NOTE: allocates ~512 MiB per OK case (then DELs) -> HEAVY, intentionally NOT
registered in parity_suite.

Usage: string_size_limit_differ.py <oracle_port> <fr_port>
"""
import socket, sys, time
def conn(p):
    s=socket.create_connection(("127.0.0.1",p),timeout=20); s.settimeout(20); return s
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def short(r): return r[:r.index(b"\r\n")] if b"\r\n" in r else r[:60]
LIM=536870912
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    def chk(label,*c):
        for s in (od,fr): cmd(s,"DEL","k")
        ro,rf=short(cmd(od,*c)),short(cmd(fr,*c))
        lo,lf=short(cmd(od,"STRLEN","k")),short(cmd(fr,"STRLEN","k"))
        if ro!=rf: fails.append(f"{label} reply: redis={ro!r} fr={rf!r}")
        if lo!=lf: fails.append(f"{label} strlen: redis={lo!r} fr={lf!r}")
        for s in (od,fr): cmd(s,"DEL","k")
    chk("setrange_at_limit","SETRANGE","k",str(LIM-2),"ab")
    chk("setrange_over_by1","SETRANGE","k",str(LIM-1),"ab")
    chk("setrange_off_at_limit","SETRANGE","k",str(LIM),"x")
    chk("setrange_off_over","SETRANGE","k",str(LIM+1),"x")
    chk("setrange_off_huge","SETRANGE","k","9999999999","x")
    # SETBIT bit-offset boundary (max bit 4294967295 -> 512 MiB string OK; >=2^32 errors)
    chk("setbit_max_bit","SETBIT","k","4294967295","1")
    chk("setbit_over_2p32","SETBIT","k","4294967296","1")
    chk("setbit_huge","SETBIT","k","99999999999999","1")
    # APPEND near the limit
    for s in (od,fr): cmd(s,"DEL","ak"); cmd(s,"SETRANGE","ak",str(LIM-3),"ab")
    ro,rf=short(cmd(od,"APPEND","ak","c")),short(cmd(fr,"APPEND","ak","c"))
    if ro!=rf: fails.append(f"append_to_limit: redis={ro!r} fr={rf!r}")
    for s in (od,fr): cmd(s,"DEL","ak"); cmd(s,"SETRANGE","ak",str(LIM-2),"ab")
    ro,rf=short(cmd(od,"APPEND","ak","c")),short(cmd(fr,"APPEND","ak","c"))
    if ro!=rf: fails.append(f"append_over: redis={ro!r} fr={rf!r}")
    for s in (od,fr): cmd(s,"DEL","ak","k")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} string size-limit divergence(s) vs redis 7.2.4:")
        for x in fails[:8]: print(f"  {x}")
        sys.exit(1)
    print("PASS — SETRANGE/APPEND proto-max-bulk-len boundary byte-exact vs redis 7.2.4 (length-checked, vs g3ioa BITFIELD offset-check)")
if __name__=="__main__": main()
