#!/usr/bin/env python3
"""Differential gate: LMPOP/ZMPOP/BLMPOP/BZMPOP family (frankenredis-...).

The multi-key pop family has subtle, distinct-message validation + selection semantics
that no other gate pins: pops from the FIRST NON-EMPTY key; COUNT (and COUNT>size);
direction LEFT/RIGHT|MIN/MAX is case-insensitive; numkeys=0 -> "wrong number of
arguments" (arity), numkeys<0 / non-int -> "numkeys should be greater than 0";
missing/bad direction -> syntax error; COUNT<=0 -> "count should be greater than 0";
numkeys > provided keys -> syntax error; WRONGTYPE; the blocking variants return
IMMEDIATELY when data is present and reject a non-float timeout ("timeout is not a
float or out of range") / negative timeout ("timeout is negative"). Byte-exact vs
redis 7.2.4.

Usage: mpop_family_differ.py <oracle_port> <fr_port>
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
    def setup():
        for s in (od,fr):
            cmd(s,"FLUSHALL"); cmd(s,"RPUSH","l2","a","b","c")
            cmd(s,"ZADD","z2","1","a","2","b","3","c"); cmd(s,"SET","strk","x")
    def chk(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        if ro!=rf: fails.append(f"{label}: redis={ro[:70]!r} fr={rf[:70]!r}")
    setup(); chk("lmpop_first_nonempty","LMPOP","2","l1","l2","LEFT")
    setup(); chk("lmpop_count","LMPOP","2","l1","l2","LEFT","COUNT","2")
    setup(); chk("lmpop_right","LMPOP","2","l2","l1","RIGHT")
    setup(); chk("lmpop_count_gt","LMPOP","1","l2","LEFT","COUNT","100")
    setup(); chk("lmpop_all_empty","LMPOP","1","l1","LEFT")
    setup(); chk("lmpop_lower","LMPOP","2","l1","l2","left")
    setup(); chk("lmpop_mixed","LMPOP","2","l1","l2","Left")
    setup()
    chk("lmpop_numkeys0","LMPOP","0","LEFT")
    chk("lmpop_numkeys_neg","LMPOP","-1","l1","LEFT")
    chk("lmpop_numkeys_nonint","LMPOP","abc","l1","LEFT")
    chk("lmpop_no_dir","LMPOP","1","l1")
    chk("lmpop_bad_dir","LMPOP","1","l1","UP")
    chk("lmpop_count0","LMPOP","1","l2","LEFT","COUNT","0")
    chk("lmpop_count_neg","LMPOP","1","l2","LEFT","COUNT","-1")
    chk("lmpop_numkeys_mismatch","LMPOP","5","l1","l2","LEFT")
    chk("lmpop_wrongtype","LMPOP","1","strk","LEFT")
    setup(); chk("zmpop_min","ZMPOP","2","z1","z2","MIN")
    setup(); chk("zmpop_max_count","ZMPOP","1","z2","MAX","COUNT","2")
    setup(); chk("zmpop_numkeys0","ZMPOP","0","MIN")
    chk("zmpop_bad_dir","ZMPOP","1","z2","MIDDLE")
    setup(); chk("zmpop_all_empty","ZMPOP","1","z1","MIN")
    setup(); chk("blmpop_data","BLMPOP","0.01","2","l1","l2","LEFT")
    setup(); chk("bzmpop_data","BZMPOP","0.01","1","z2","MIN","COUNT","2")
    chk("blmpop_badtimeout","BLMPOP","abc","1","l2","LEFT")
    chk("blmpop_negtimeout","BLMPOP","-1","1","l2","LEFT")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} MPOP-family divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        sys.exit(1)
    print("PASS — LMPOP/ZMPOP/BLMPOP/BZMPOP family byte-exact vs redis 7.2.4 (selection + COUNT + direction + numkeys/count/timeout validation messages)")
if __name__=="__main__": main()
