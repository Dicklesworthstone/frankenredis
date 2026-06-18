#!/usr/bin/env python3
"""Differential gate: non-UTF8 argument handling parity (frankenredis-44iva).

redis works on raw bytes; many fr command paths convert args to &str. This gate locks
the cases where fr ALREADY matches redis for a non-UTF8 (\xff\xfe) argument, and
REPORTS (does not assert) the known 44iva divergence so it flips to a hard assertion
once fixed:
  MATCH (asserted): non-UTF8 in a numeric VALUE position -> "value is not an integer"
  (RESTORE ttl, RESTORE IDLETIME value); GETEX with a non-UTF8 option -> "syntax
  error"; trailing UTF-8 garbage option on RESTORE -> "syntax error"; SETRANGE/GETRANGE
  numeric arg non-UTF8 -> not-integer.
  KNOWN (44iva, reported): SET k v <non-UTF8> and RESTORE ... <non-UTF8 trailing> ->
  redis "syntax error" but fr "invalid UTF-8 argument" (option loop UTF-8-validates
  before the syntax check; GETEX matches bytes and is correct).

Usage: nonutf8_arg_parity_differ.py <oracle_port> <fr_port>
       Exit 0 = the asserted (correct) cases byte-exact, 1 = a NEW divergence.
"""
import socket, sys, time
NUTF=b"\xff\xfe"
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a:
        x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def dump(s,key): r=cmd(s,"DUMP",key); nl=r.index(b"\r\n"); return r[nl+2:nl+2+int(r[1:nl])]
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]; known=[]
    for s in (od,fr): cmd(s,"FLUSHALL"); cmd(s,"SET","src","hello")
    pl=dump(od,"src")
    def cmp(label, args, assert_it):
        ro,rf=cmd(od,*args),cmd(fr,*args)
        if ro!=rf:
            (fails if assert_it else known).append(f"{label}: redis={ro[:50]!r} fr={rf[:50]!r}")
    # asserted (fr already correct)
    cmp("restore_ttl_nonutf8",[b"RESTORE",b"a",NUTF,pl],True)
    cmp("restore_idletime_val_nonutf8",[b"RESTORE",b"b",b"0",pl,b"IDLETIME",NUTF],True)
    cmp("getex_opt_nonutf8",[b"GETEX",b"src",NUTF],True)
    cmp("restore_trailing_utf8_garbage",[b"RESTORE",b"c",b"0",pl,b"BOGUS"],True)
    cmp("setrange_off_nonutf8",[b"SETRANGE",b"d",NUTF,b"x"],True)
    cmp("getrange_idx_nonutf8",[b"GETRANGE",b"src",NUTF,b"1"],True)
    cmp("expire_ttl_nonutf8",[b"EXPIRE",b"src",NUTF],True)
    # known divergence (44iva) — reported, not asserted
    cmp("set_opt_nonutf8",[b"SET",b"e",b"v",NUTF],False)
    cmp("restore_trailing_nonutf8",[b"RESTORE",b"f",b"0",pl,NUTF],False)
    cmp("restore_replace_then_nonutf8",[b"RESTORE",b"g",b"0",pl,b"REPLACE",NUTF],False)
    print("="*60)
    if known:
        print("KNOWN (frankenredis-44iva, not asserted): "+"; ".join(known))
    if fails:
        print(f"FAIL — {len(fails)} NEW non-UTF8 arg divergence(s) vs redis 7.2.4:")
        for x in fails[:10]: print(f"  {x}")
        sys.exit(1)
    print("PASS — non-UTF8 numeric/value + GETEX-option handling byte-exact vs redis 7.2.4 (SET/RESTORE option-token divergence pending 44iva)")
if __name__=="__main__": main()
