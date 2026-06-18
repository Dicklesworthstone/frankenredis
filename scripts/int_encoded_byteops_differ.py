#!/usr/bin/env python3
"""Differential gate: byte-level ops on integer-encoded strings (frankenredis-...).

A SET of a canonical integer is `int`-encoded (a shared/embedded long, not a byte
buffer). Byte-level commands must transparently materialize it: READ ops
(GETRANGE/GETBIT/BITCOUNT/STRLEN) return the decimal-string bytes and LEAVE the
encoding `int`; WRITE ops (SETRANGE/SETBIT/APPEND) convert to `raw` with byte-exact
content (e.g. SETBIT bit0 of "1"=0x31 -> 0xb1; SETRANGE past the end zero-pads). An
EMPTY SETRANGE is a no-op that must NOT trigger the raw transition. GETEX with no
option leaves the encoding `int`. This pins all of that byte-exact vs redis 7.2.4
(complements string_encoding_differ, which covers SET-time encoding + write->raw, not
the read-keeps-int / no-op-no-transition / materialized-content aspects).

Usage: int_encoded_byteops_differ.py <oracle_port> <fr_port>
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
        if ro!=rf: fails.append(f"{label}: redis={ro[:60]!r} fr={rf[:60]!r}")
    def ri(key="k",val="12345"): each("DEL",key); each("SET",key,val)
    each("FLUSHALL")
    ri(); chk("enc_int","OBJECT","ENCODING","k")
    chk("getrange","GETRANGE","k","0","2"); chk("getrange_neg","GETRANGE","k","-2","-1")
    chk("strlen","STRLEN","k"); chk("getbit","GETBIT","k","0"); chk("bitcount","BITCOUNT","k")
    chk("enc_after_read","OBJECT","ENCODING","k")
    ri(); chk("setrange","SETRANGE","k","1","9"); chk("get_setrange","GET","k"); chk("enc_setrange","OBJECT","ENCODING","k")
    ri(); chk("setbit","SETBIT","k","0","1"); chk("get_setbit","GET","k"); chk("enc_setbit","OBJECT","ENCODING","k")
    ri(); chk("append","APPEND","k","67"); chk("get_append","GET","k"); chk("enc_append","OBJECT","ENCODING","k")
    ri(); chk("setrange_extend","SETRANGE","k","8","Z"); chk("get_extend","GET","k"); chk("strlen_extend","STRLEN","k")
    ri(); chk("setrange_empty","SETRANGE","k","0",""); chk("enc_empty_setrange","OBJECT","ENCODING","k")
    ri(); chk("getex_noopt","GETEX","k"); chk("enc_getex","OBJECT","ENCODING","k")
    each("DEL","ni"); each("SET","ni","-9876")
    chk("getrange_negint","GETRANGE","ni","0","1"); chk("getbit_negint","GETBIT","ni","2")
    each("DEL","ic"); each("SET","ic","99"); each("INCR","ic")
    chk("enc_after_incr","OBJECT","ENCODING","ic"); chk("getrange_after_incr","GETRANGE","ic","0","-1")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} int-encoded byte-op divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        sys.exit(1)
    print("PASS — integer-encoded byte ops byte-exact vs redis 7.2.4 (reads keep int+materialize, writes->raw exact, empty-setrange no-op, getex keeps int)")
if __name__=="__main__": main()
