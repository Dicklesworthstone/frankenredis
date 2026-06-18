#!/usr/bin/env python3
"""Differential gate: expiration timestamp edges (frankenredis-...).

Pins the subtle create-then-expire + invalid-expire semantics byte-exact vs redis
7.2.4: SET/GETEX with EXAT/PXAT in the PAST create the key then immediately expire it
(reply OK / old value, but EXISTS=0); EX/PX/EXAT/PXAT of 0 or negative reject with
"invalid expire time in '<cmd>'" (command name embedded); GETEX with a past abs time
returns the old value then deletes; EXPIRE/EXPIREAT with a past/negative time delete
the key (reply 1); SETEX/PSETEX with 0 reject; PEXPIRE with a large ms succeeds while
EXPIRE with the same value in SECONDS overflows the ms timestamp and errors; the
EXPIRE GT/LT/NX/XX conditional flags apply correctly. (Complements expire_overflow /
getex_ttl / ttl_on_mutation, which cover overflow values + TTL readback, not the
past-timestamp delete semantics + per-command error wording.)

Usage: expire_timestamp_edge_differ.py <oracle_port> <fr_port>
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
        if ro!=rf: fails.append(f"{label}: redis={ro[:70]!r} fr={rf[:70]!r}")
    def cleanup():
        for s in (od,fr):
            try: cmd(s,"FLUSHALL")
            except Exception: pass
    each("FLUSHALL")
    each("DEL","k"); chk("set_exat_past","SET","k","v","EXAT","1"); chk("exists_exat_past","EXISTS","k"); chk("get_exat_past","GET","k")
    each("DEL","k"); chk("set_pxat_past","SET","k","v","PXAT","1"); chk("exists_pxat_past","EXISTS","k")
    each("DEL","k"); chk("set_exat_zero","SET","k","v","EXAT","0"); chk("exists_exat_zero","EXISTS","k")
    chk("set_ex_zero","SET","z1","v","EX","0"); chk("set_ex_neg","SET","z2","v","EX","-1"); chk("set_px_neg","SET","z3","v","PX","-100")
    each("DEL","g"); each("SET","g","val"); chk("getex_exat_past","GETEX","g","EXAT","1"); chk("exists_getex_past","EXISTS","g")
    each("DEL","g2"); each("SET","g2","val"); chk("getex_ex_zero","GETEX","g2","EX","0"); chk("exists_g2","EXISTS","g2")
    each("DEL","e"); each("SET","e","v"); chk("expire_neg","EXPIRE","e","-1"); chk("exists_expire_neg","EXISTS","e")
    each("DEL","e2"); each("SET","e2","v"); chk("expireat_past","EXPIREAT","e2","1"); chk("exists_expireat_past","EXISTS","e2")
    each("DEL","e3"); each("SET","e3","v"); chk("pexpire_huge_ok","PEXPIRE","e3","9999999999999999"); chk("expire_huge_overflow","EXPIRE","e3","9999999999999999")
    chk("setex_zero","SETEX","sx","0","v"); chk("psetex_zero","PSETEX","sx","0","v"); chk("setex_neg","SETEX","sx","-5","v")
    each("DEL","ef"); each("SET","ef","v"); each("EXPIRE","ef","100")
    chk("expire_gt_lower","EXPIRE","ef","50","GT"); chk("expire_lt_lower","EXPIRE","ef","50","LT")
    chk("expire_nx_existing","EXPIRE","ef","200","NX"); chk("expire_xx_existing","EXPIRE","ef","300","XX")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} expire timestamp-edge divergence(s) vs redis 7.2.4:")
        for x in fails[:14]: print(f"  {x}")
        cleanup()
        sys.exit(1)
    cleanup()
    print("PASS — expire timestamp edges byte-exact vs redis 7.2.4 (EXAT/PXAT past-delete, invalid-expire per-cmd wording, GETEX-past, EXPIRE neg/flags, PEXPIRE/EXPIRE overflow asymmetry)")
if __name__=="__main__": main()
