#!/usr/bin/env python3
"""Regression gate for frankenredis-g3ioa: BITFIELD offset-limit boundary.

redis bounds BITFIELD by the OFFSET byte-index ((bit_offset>>3) >= proto_max_bulk_len
-> "bit offset is not an integer or out of range"), NOT the final string length, so a
field at the highest addressable offset may grow the string a few bytes past
proto-max-bulk-len. fr previously length-checked needed_bytes>512MiB and wrongly
rejected offsets 4294967289..4294967295. This pins the boundary byte-exact vs redis.

NOTE: each successful case allocates a ~512 MiB string (then DELs it), so this gate
is HEAVY and intentionally NOT registered in parity_suite (it would bloat the
release-readiness suite). Run standalone after touching the BITFIELD size guard.
The normal BITFIELD surface is covered by bitfield_overflow_differ /
bitcount_bitpos_range_differ in the suite.

Usage: bitfield_offset_limit_differ.py <oracle_port> <fr_port>
"""
import socket, sys, time
def conn(p):
    s=socket.create_connection(("127.0.0.1",p),timeout=20); s.settimeout(20); return s
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def short(r): return r[:r.index(b"\r\n")] if b"\r\n" in r else r[:60]
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp)
    fails=[]
    # (offset, expect: 'ok' just below/at boundary, 'err' offset out of range)
    for off in [4294967288, 4294967289, 4294967295, 4294967296]:
        for s in (od,fr): cmd(s,"DEL","bfk")
        ro=short(cmd(od,"BITFIELD","bfk","SET","u8",str(off),"1"))
        rf=short(cmd(fr,"BITFIELD","bfk","SET","u8",str(off),"1"))
        lo=short(cmd(od,"STRLEN","bfk")); lf=short(cmd(fr,"STRLEN","bfk"))
        if ro!=rf: fails.append(f"off={off} reply: redis={ro!r} fr={rf!r}")
        if lo!=lf: fails.append(f"off={off} strlen: redis={lo!r} fr={lf!r}")
        for s in (od,fr): cmd(s,"DEL","bfk")
    # i16 at the boundary (different bits width)
    for s in (od,fr): cmd(s,"DEL","bfk")
    ro=short(cmd(od,"BITFIELD","bfk","SET","i16","4294967280","1"))
    rf=short(cmd(fr,"BITFIELD","bfk","SET","i16","4294967280","1"))
    if ro!=rf: fails.append(f"i16_boundary: redis={ro!r} fr={rf!r}")
    for s in (od,fr): cmd(s,"DEL","bfk")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} BITFIELD offset-limit divergence(s) vs redis 7.2.4:")
        for x in fails[:8]: print(f"  {x}")
        sys.exit(1)
    print("PASS — BITFIELD offset-limit boundary byte-exact vs redis 7.2.4 (g3ioa fixed)")
if __name__=="__main__": main()
