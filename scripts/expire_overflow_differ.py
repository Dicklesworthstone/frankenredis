#!/usr/bin/env python3
"""Differential gate: expire-time + float overflow edges (frankenredis-wkyo7).

TTL commands must reject expire times whose internal absolute-millisecond
computation overflows i64 ("invalid expire time" error) while treating an
already-past absolute time as an immediate delete; and INCRBYFLOAT/HINCRBYFLOAT
must reject a result that is NaN/Infinity. Both are arithmetic-edge classes with
byte-exact error wording. This gate pins them vs redis 7.2.4.

Usage: expire_overflow_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

IMAX = "9223372036854775807"
IMIN = "-9223372036854775808"
HUGE = "9999999999999999"  # seconds; *1000 overflows i64


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

    def reset():
        for s in (od, fr):
            cmd(s, "FLUSHALL")
            cmd(s, "SET", "k", "v")

    # seconds-based commands: *1000 + now overflows -> invalid expire time
    reset()
    chk("expire_huge", "EXPIRE", "k", HUGE)
    chk("expire_imax", "EXPIRE", "k", IMAX)
    chk("expireat_imax", "EXPIREAT", "k", IMAX)
    chk("setex_huge", "SETEX", "k", HUGE, "v")
    chk("setex_imax", "SETEX", "k", IMAX, "v")
    chk("set_ex_huge", "SET", "k", "v", "EX", HUGE)
    chk("set_exat_imax", "SET", "k", "v", "EXAT", IMAX)
    chk("getex_ex_huge", "GETEX", "k", "EX", HUGE)
    chk("getex_exat_imax", "GETEX", "k", "EXAT", IMAX)
    # millisecond-based: now + imax overflows
    reset()
    chk("pexpire_imax", "PEXPIRE", "k", IMAX)
    chk("pexpireat_imax", "PEXPIREAT", "k", IMAX)
    chk("psetex_imax", "PSETEX", "k", IMAX, "v")
    chk("set_px_imax", "SET", "k", "v", "PX", IMAX)
    # zero / negative relative TTL on SETEX/PSETEX -> invalid expire time error
    reset()
    chk("setex_zero", "SETEX", "k", "0", "v")
    chk("setex_neg", "SETEX", "k", "-1", "v")
    chk("psetex_zero", "PSETEX", "k", "0", "v")
    # already-past absolute / very-negative relative -> immediate delete (not error)
    reset()
    chk("expireat_0", "EXPIREAT", "k", "0")
    chk("expireat_0_gone", "EXISTS", "k")
    reset()
    chk("expireat_neg", "EXPIREAT", "k", "-1")
    chk("expireat_neg_gone", "EXISTS", "k")
    reset()
    chk("pexpire_neg", "PEXPIRE", "k", "-1")
    chk("pexpire_neg_gone", "EXISTS", "k")
    reset()
    chk("expire_imin", "EXPIRE", "k", IMIN)
    chk("expire_imin_state", "EXISTS", "k")
    # float overflow -> "increment would produce NaN or Infinity"
    for s in (od, fr):
        cmd(s, "SET", "fl", "1e308")
    chk("incrbyfloat_overflow", "INCRBYFLOAT", "fl", "1e308")
    for s in (od, fr):
        cmd(s, "SET", "fl2", "3.0e3")
    chk("incrbyfloat_ok", "INCRBYFLOAT", "fl2", "200")
    chk("incrbyfloat_nan", "INCRBYFLOAT", "fl2", "nan")
    chk("incrbyfloat_inf_arg", "INCRBYFLOAT", "fl2", "inf")
    for s in (od, fr):
        cmd(s, "DEL", "h")
        cmd(s, "HSET", "h", "f", "1e308")
    chk("hincrbyfloat_overflow", "HINCRBYFLOAT", "h", "f", "1e308")
    chk("hincrbyfloat_nan", "HINCRBYFLOAT", "h", "f", "nan")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} expire/float-overflow divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — expire-time + float overflow byte-exact vs redis 7.2.4 "
        "(invalid-expire on overflow/zero/neg-rel, past=delete, INCRBYFLOAT/HINCRBYFLOAT NaN/Inf)"
    )


if __name__ == "__main__":
    main()
