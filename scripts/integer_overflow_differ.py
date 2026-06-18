#!/usr/bin/env python3
"""Differential gate: integer / bit-boundary overflow errors (frankenredis-hk1jl).

Arithmetic and bit/range commands must detect i64 overflow and out-of-bounds the
same way redis does, with byte-identical error wording — a classic parity-bug
source. Covers:
  * INCR/DECR/INCRBY/DECRBY past i64 max/min (incl. DECRBY of i64-min, whose negation
    overflows), argument-not-an-integer / argument-overflows-i64, leading-space reject
  * HINCRBY hash-field overflow
  * SETBIT value not 0/1, negative offset, offset >= 4*1024*1024*1024 bits
  * SETRANGE offset+len exceeding the 512 MiB proto limit
plus the non-error baselines (a valid INCRBY of i64-min onto 0, APPEND-forces-raw).

Usage: integer_overflow_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

IMAX = "9223372036854775807"
IMIN = "-9223372036854775808"


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.02)
    return s.recv(1 << 20)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    def setk(k, v):
        for s in (od, fr):
            cmd(s, "SET", k, v)

    for s in (od, fr):
        cmd(s, "FLUSHALL")

    setk("a", IMAX); chk("incr_overflow", "INCR", "a")
    setk("b", IMIN); chk("decr_overflow", "DECR", "b")
    setk("c", IMAX); chk("incrby_overflow", "INCRBY", "c", "1")
    setk("d", IMIN); chk("decrby_overflow", "DECRBY", "d", "1")
    setk("e", "10"); chk("incrby_by_max", "INCRBY", "e", IMAX)
    setk("f", "0"); chk("incrby_imin_ok", "INCRBY", "f", IMIN)
    chk("incrby_imin_ok_val", "GET", "f")
    setk("g", "0"); chk("decrby_imin_overflow", "DECRBY", "g", IMIN)   # negating i64-min overflows
    chk("incrby_arg_overflow", "INCRBY", "h", "99999999999999999999")
    setk("i", "3.5"); chk("incr_nonint", "INCR", "i")
    setk("j", "  5"); chk("incr_leading_space", "INCR", "j")
    setk("k", "5 "); chk("incr_trailing_space", "INCR", "k")
    # HINCRBY field overflow
    for s in (od, fr):
        cmd(s, "DEL", "hh")
        cmd(s, "HSET", "hh", "ff", IMAX)
    chk("hincrby_overflow", "HINCRBY", "hh", "ff", "1")
    chk("hincrby_nonint_field", "HSET", "hh", "nf", "x")
    chk("hincrby_nonint", "HINCRBY", "hh", "nf", "1")
    # SETBIT value / offset bounds
    setk("sb", "x")
    chk("setbit_badval", "SETBIT", "sb", "0", "2")
    chk("setbit_neg_offset", "SETBIT", "sb", "-1", "1")
    chk("setbit_huge_offset", "SETBIT", "sb", "4294967296", "1")
    chk("setbit_ok", "SETBIT", "sbm", "100", "1")
    chk("setbit_ok_strlen", "STRLEN", "sbm")
    # SETRANGE proto-limit
    chk("setrange_huge", "SETRANGE", "sr", "536870912", "x")
    chk("setrange_pad_ok", "SETRANGE", "sr2", "5", "hi")
    chk("setrange_pad_val", "GET", "sr2")
    chk("setrange_neg", "SETRANGE", "sr3", "-1", "x")
    # APPEND forces raw encoding off an int-encoded key
    for s in (od, fr):
        cmd(s, "DEL", "ap")
        cmd(s, "SET", "ap", "12345")
    chk("append_int", "APPEND", "ap", "6")
    chk("append_int_enc", "OBJECT", "ENCODING", "ap")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} integer/bit-boundary divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — integer/bit-boundary overflow errors byte-exact vs redis 7.2.4 "
        "(INCR-family/HINCRBY i64 overflow + SETBIT/SETRANGE bounds + APPEND-raw)"
    )


if __name__ == "__main__":
    main()
