#!/usr/bin/env python3
"""Differential gate: stream ID validation + trim semantics (frankenredis-...).

Deterministic stream-ID and trimming surface byte-exact vs redis 7.2.4: XADD with an
explicit ID enforces strictly-increasing order ("equal or smaller than the target
stream top item"), the 0-0 special case ("must be greater than 0-0", even on an empty
stream), ms-* auto-sequence, NOMKSTREAM on a missing key (nil, no create); XSETID
rejects a smaller ID ("smaller than the target stream top item"), accepts
ENTRIESADDED/MAXDELETEDID, and FORCE is a syntax error in 7.2.4; MAXLEN exact trimming
keeps the newest N, MAXLEN ~ does NOT trim below the radix-node threshold, and an
explicit LIMIT caps whole-node removal (kept counts are deterministic for a fixed
insert sequence); MINID trims by id, XTRIM MAXLEN/MINID return the trimmed count.
(The approximate-MINID cases also cover XADD thresholds at or below the stream's
first ID plus standalone `XTRIM key MINID ~ 0-0`, where trimming is provably a
no-op.)
(Complements stream_xinfo / stream_command_fuzz with deterministic ID-error wording
and trim counts.)

Usage: stream_id_trim_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import re
import socket
import sys
import time

STREAM_ID_BULK = re.compile(rb"\$\d+\r\n\d+-\d+\r\n")

def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def run_differ(od, fr):
    fails=[]
    def each(*c):
        for s in (od,fr): cmd(s,*c)
    def chk(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        if ro!=rf: fails.append(f"{label}: redis={ro[:80]!r} fr={rf[:80]!r}")
    def chk_auto_ids(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        normalized_oracle, oracle_ids = STREAM_ID_BULK.subn(b"$ID\r\n", ro)
        normalized_fr, fr_ids = STREAM_ID_BULK.subn(b"$ID\r\n", rf)
        if oracle_ids == 0 or fr_ids == 0 or normalized_oracle != normalized_fr:
            fails.append(f"{label}: redis={ro[:80]!r} fr={rf[:80]!r}")
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
    each("DEL","auto2")
    chk_auto_ids("xadd_auto_two_fields","XADD","auto2","*","f1","v1","f2","v2")
    chk_auto_ids("xrange_auto_two_fields","XRANGE","auto2","-","+")
    each("DEL","nomk_existing")
    each("XADD","nomk_existing","1000-0","seed","value")
    chk_auto_ids("xadd_nomkstream_existing","XADD","nomk_existing","NOMKSTREAM","*","f","v")
    chk_auto_ids("xrange_nomkstream_existing","XRANGE","nomk_existing","-","+")
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
    each("DEL","ml2_fields")
    for _ in range(250):
        chk_auto_ids("xadd_maxlen_approx_two_fields","XADD","ml2_fields","MAXLEN","~","100","*","f1","v1","f2","v2")
    chk("xlen_maxlen_approx_two_fields","XLEN","ml2_fields")
    chk_auto_ids("xrange_maxlen_approx_two_fields","XRANGE","ml2_fields","-","+")
    each("DEL","ml_limit")
    for _ in range(250): each("XADD","ml_limit","MAXLEN","~","100","LIMIT","100","*","f","v")
    chk("xlen_maxlen_approx_limit100","XLEN","ml_limit")
    each("DEL","ml_limit_small")
    for _ in range(250): each("XADD","ml_limit_small","MAXLEN","~","100","LIMIT","50","*","f","v")
    chk("xlen_maxlen_approx_limit50","XLEN","ml_limit_small")
    chk("xadd_limit_negative","XADD","ml_err","MAXLEN","~","100","LIMIT","-1","*","f","v")
    chk("xadd_limit_nonint","XADD","ml_err","MAXLEN","~","100","LIMIT","x","*","f","v")
    chk("xadd_limit_requires_approx","XADD","ml_err","MAXLEN","=","100","LIMIT","100","*","f","v")
    chk("xadd_limit_wrong_keyword","XADD","ml_err","MAXLEN","~","100","BOGUS","100","*","f","v")
    each("DEL","mi")
    for i in range(1,11): each("XADD","mi",f"{i}-0","f","v")
    each("XADD","mi","MINID","5","11-0","f","v")
    chk("xlen_minid","XLEN","mi"); chk("xrange_minid","XRANGE","mi","-","+")
    each("DEL","mi_stale")
    for i in range(1000,1250): each("XADD","mi_stale",f"{i}-0","f","v")
    chk("xadd_minid_approx_below_first","XADD","mi_stale","MINID","~","999-0","LIMIT","10000","2000-0","f","v")
    chk("xadd_minid_approx_at_first","XADD","mi_stale","MINID","~","1000-0","LIMIT","10000","2001-0","f","v")
    chk("xtrim_minid_approx_zero_noop","XTRIM","mi_stale","MINID","~","0-0")
    chk("xlen_minid_approx_stale","XLEN","mi_stale")
    chk("xrange_minid_approx_stale","XRANGE","mi_stale","-","+","COUNT","2")
    chk("xtrim_maxlen3","XTRIM","mi","MAXLEN","3"); chk("xlen_trim3","XLEN","mi")
    chk("xtrim_minid_all","XTRIM","mi","MINID","20"); chk("xlen_trim_all","XLEN","mi")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} stream ID/trim divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        sys.exit(1)
    print("PASS — stream ID-validation + trim semantics byte-exact vs redis 7.2.4 (XADD order/0-0/NOMKSTREAM, XSETID, MAXLEN exact+approx+LIMIT, MINID, XTRIM)")


def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    with (
        socket.create_connection(("127.0.0.1", op), timeout=5) as od,
        socket.create_connection(("127.0.0.1", fp), timeout=5) as fr,
    ):
        run_differ(od, fr)


if __name__=="__main__": main()
