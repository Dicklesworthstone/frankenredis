#!/usr/bin/env python3
"""Differential gate: proto-max-bulk-len config respect (frankenredis-uwhyl).

fr's string/bit size checks must honor the CONFIGURED proto-max-bulk-len, not a
hardcoded 512 MiB. With proto-max-bulk-len set below the default, APPEND/SETRANGE
reject a write whose RESULT exceeds the limit, and SETBIT/BITFIELD reject a bit offset
whose byte index reaches the limit — byte-exact vs redis 7.2.4. At the default 512 MiB
the SETBIT/BITFIELD boundary (bit 4294967295 ok / 4294967296 out-of-range) is
unchanged. Sets the knob to 1 MiB + 4 MiB, checks over/at-limit for each command, then
restores the default 536870912 (suite-safe). Locks the uwhyl fix (90331d584).

Usage: config_size_limit_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=8)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def short(r): return r[:r.index(b"\r\n")] if b"\r\n" in r else r[:60]
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    def chk(label,*c,cleanup="csk"):
        for s in (od,fr): cmd(s,"DEL",cleanup)
        ro,rf=short(cmd(od,*c)),short(cmd(fr,*c))
        if ro!=rf: fails.append(f"{label}: redis={ro!r} fr={rf!r}")
        for s in (od,fr): cmd(s,"DEL",cleanup)
    try:
        for M in (1048576, 4194304):
            for s in (od,fr): cmd(s,"CONFIG","SET","proto-max-bulk-len",str(M))
            chk(f"setrange_over@{M}","SETRANGE","csk",str(M-1),"ab")
            chk(f"setrange_off@{M}","SETRANGE","csk",str(M),"x")
            chk(f"setrange_at@{M}","SETRANGE","csk",str(M-2),"ab")
            chk(f"setbit_over@{M}","SETBIT","csk",str(M*8),"1")
            chk(f"setbit_at@{M}","SETBIT","csk",str(M*8-1),"1")
            chk(f"setbit_under@{M}","SETBIT","csk",str(M*8-8),"1")
            chk(f"bitfield_over@{M}","BITFIELD","csk","SET","u8",str(M*8),"1")
            # APPEND over: build base then append
            for s in (od,fr): cmd(s,"DEL","cak"); cmd(s,"SETRANGE","cak",str(M-2),"ab")
            ro,rf=short(cmd(od,"APPEND","cak","c")),short(cmd(fr,"APPEND","cak","c"))
            if ro!=rf: fails.append(f"append_over@{M}: redis={ro!r} fr={rf!r}")
            for s in (od,fr): cmd(s,"DEL","cak")
        # default-config boundary regression
        for s in (od,fr): cmd(s,"CONFIG","SET","proto-max-bulk-len","536870912")
        chk("setbit_512m_lastvalid","SETBIT","csk","4294967295","1")
        chk("setbit_512m_oob","SETBIT","csk","4294967296","1")
    finally:
        for s in (od,fr): cmd(s,"CONFIG","SET","proto-max-bulk-len","536870912"); cmd(s,"DEL","csk","cak")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} proto-max-bulk-len config divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        sys.exit(1)
    print("PASS — APPEND/SETRANGE/SETBIT/BITFIELD honor proto-max-bulk-len (1MiB/4MiB) + default boundary unchanged, byte-exact vs redis 7.2.4 (uwhyl)")
if __name__=="__main__": main()
