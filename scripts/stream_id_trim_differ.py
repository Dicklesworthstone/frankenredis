#!/usr/bin/env python3
"""Differential gate: stream ID validation + trim semantics (frankenredis-...).

Deterministic stream-ID and trimming surface byte-exact vs redis 7.2.4: XADD with an
explicit ID enforces strictly-increasing order ("equal or smaller than the target
stream top item"), the 0-0 special case ("must be greater than 0-0", even on an empty
stream), ms-* auto-sequence, NOMKSTREAM on a missing key (nil, no create); XSETID
rejects a smaller ID ("smaller than the target stream top item"), accepts
ENTRIESADDED/MAXDELETEDID, and FORCE is a syntax error in 7.2.4; MAXLEN exact trimming
keeps the newest N, MAXLEN ~ does NOT trim below the radix-node threshold (kept count
is deterministic for a fixed insert sequence), MINID trims by id, XTRIM MAXLEN/MINID
return the trimmed count. (Complements stream_xinfo / stream_command_fuzz with the
deterministic ID-error wording + trim counts.)

Usage: stream_id_trim_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    def each(*c):
        for s in (od,fr): cmd(s,*c)
    def chk(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        if ro!=rf: fails.append(f"{label}: redis={ro[:80]!r} fr={rf[:80]!r}")
    each("FLUSHALL")
    each("DEL","st")
    chk("xadd_explicit","XADD","st","5-5","f","v")
    chk("xadd_lower_err","XADD","st","5-4","f","v")
    chk("xadd_equal_err","XADD","st","5-5","f","v")
    chk("xadd_ms_autoseq","XADD","st","5-*","f","v")
    chk("xadd_zero_err","XADD","st","0-0","f","v")
    each("DEL","st2")
    chk("xadd_00_empty","XADD","st2","0-0","f","v")
    chk("xadd_01_empty","XADD","st2","0-1","f","v")
    chk("xadd_nomkstream","XADD","nope","NOMKSTREAM","*","f","v"); chk("nomk_noexist","EXISTS","nope")
    each("DEL","s3"); each("XADD","s3","10-0","f","v")
    chk("xsetid_lower_err","XSETID","s3","5-0")
    chk("xsetid_higher","XSETID","s3","20-0")
    chk("xsetid_force_syntax","XSETID","s3","5-0","FORCE")
    chk("xsetid_entriesadded","XSETID","s3","25-0","ENTRIESADDED","100","MAXDELETEDID","3-0")
    each("DEL","ml")
    for i in range(1,11): each("XADD","ml","MAXLEN",str(5),f"{i}-0","f","v")
    chk("xlen_maxlen5","XLEN","ml"); chk("xrange_maxlen","XRANGE","ml","-","+")
    each("DEL","ml2")
    for i in range(1,21): each("XADD","ml2","MAXLEN","~",str(5),f"{i}-0","f","v")
    chk("xlen_maxlen_approx","XLEN","ml2")
    each("DEL","mi")
    for i in range(1,11): each("XADD","mi",f"{i}-0","f","v")
    each("XADD","mi","MINID","5","11-0","f","v")
    chk("xlen_minid","XLEN","mi"); chk("xrange_minid","XRANGE","mi","-","+")
    chk("xtrim_maxlen3","XTRIM","mi","MAXLEN","3"); chk("xlen_trim3","XLEN","mi")
    chk("xtrim_minid_all","XTRIM","mi","MINID","20"); chk("xlen_trim_all","XLEN","mi")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} stream ID/trim divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        sys.exit(1)
    print("PASS — stream ID-validation + trim semantics byte-exact vs redis 7.2.4 (XADD order/0-0/NOMKSTREAM, XSETID, MAXLEN exact+approx, MINID, XTRIM)")
if __name__=="__main__": main()
