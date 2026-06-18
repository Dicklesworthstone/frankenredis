#!/usr/bin/env python3
"""Differential gate: SADD intset canonical-int eligibility (frankenredis-ja3xy).

A set is intset-encoded only when EVERY member is a CANONICAL i64 decimal string
(string2ll round-trip). Non-canonical int-looking members must stay listpack STRING
members with their exact bytes preserved: leading-zero "007", "+5", leading/trailing
space, "7.0", "0x10", "-0", lone "-", empty, and values past i64 range. Canonical i64
(incl. min/max) -> intset; adding a non-canonical member to an intset transitions it
to listpack while preserving all members verbatim. Byte-exact vs redis 7.2.4. This
predicate underpins bbyfz (the RESTORE re-derive must use it, not a raw parse_i64).

Usage: set_intset_canonical_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import re, socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def enc(s,k):
    r=cmd(s,"OBJECT","ENCODING",k); return r[r.index(b"\r\n")+2:].split(b"\r\n")[0] if r.startswith(b"$") and b"$-1" not in r[:4] else b"?"
def members(b): return tuple(sorted(re.findall(rb"\$\d+\r\n([^\r]*)\r\n", b)))
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    for s in (od,fr):
        cmd(s,"FLUSHALL"); cmd(s,"CONFIG","SET","set-max-intset-entries","512"); cmd(s,"CONFIG","SET","set-max-listpack-entries","128")
    def chk(label,*adds):
        for s in (od,fr): cmd(s,"DEL","k"); cmd(s,"SADD","k",*adds)
        eo,ef=enc(od,"k"),enc(fr,"k"); mo,mf=members(cmd(od,"SMEMBERS","k")),members(cmd(fr,"SMEMBERS","k"))
        if eo!=ef: fails.append(f"{label} enc: redis={eo.decode()} fr={ef.decode()}")
        if mo!=mf: fails.append(f"{label} members: redis={mo} fr={mf}")
    chk("canonical_ints","1","2","3")
    chk("leading_zero","007","8","9"); chk("plus_sign","+5","6"); chk("leading_space"," 5","6")
    chk("trailing_space","5 ","6"); chk("float_like","7.0","8"); chk("hex_like","0x10","8")
    chk("neg_zero","-0","1"); chk("just_minus","-","1"); chk("empty_member","","1")
    chk("huge_overflow","99999999999999999999","1"); chk("i64_overflow_by1","9223372036854775808","2")
    chk("i64_min","-9223372036854775808","1"); chk("i64_max","9223372036854775807","2")
    chk("mixed_canon_noncanon","1","2","007")
    # intset -> listpack transition on non-canonical add (members preserved)
    for s in (od,fr): cmd(s,"DEL","g"); cmd(s,"SADD","g","1","2","3")
    e1o,e1f=enc(od,"g"),enc(fr,"g")
    for s in (od,fr): cmd(s,"SADD","g","007")
    e2o,e2f=enc(od,"g"),enc(fr,"g"); m2o,m2f=members(cmd(od,"SMEMBERS","g")),members(cmd(fr,"SMEMBERS","g"))
    if (e1o,e2o)!=(e1f,e2f): fails.append(f"transition enc: redis={e1o.decode()}/{e2o.decode()} fr={e1f.decode()}/{e2f.decode()}")
    if m2o!=m2f: fails.append(f"transition members: redis={m2o} fr={m2f}")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} SADD canonical-int divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        sys.exit(1)
    print("PASS — SADD intset canonical-int eligibility byte-exact vs redis 7.2.4 (non-canonical stay listpack-string, canonical i64 -> intset, transition preserves members)")
if __name__=="__main__": main()
