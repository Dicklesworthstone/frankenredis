#!/usr/bin/env python3
"""Differential gate: MSETNX atomicity + SETNX/MSET (frankenredis-66hi6).

MSETNX is all-or-nothing: if ANY target key already exists, NONE of the pairs are
written and it returns 0. This atomicity (and the dup-key-within-one-call behavior,
the odd-args arity error, and the contrast with always-overwriting MSET) is subtle
and bug-prone. Pins it byte-exact vs redis 7.2.4 — checking both the reply and the
resulting per-key EXISTS/GET state.

Usage: msetnx_atomicity_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time


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

    def flush():
        for s in (od, fr):
            cmd(s, "FLUSHALL")

    # all new -> 1, all written
    flush()
    chk("allnew_reply", "MSETNX", "a", "1", "b", "2", "c", "3")
    chk("allnew_a", "GET", "a")
    chk("allnew_b", "GET", "b")
    chk("allnew_c", "GET", "c")
    # one key already exists -> 0, and NONE of the new keys are written (atomic)
    chk("partial_reply", "MSETNX", "d", "4", "a", "99", "e", "5")
    chk("partial_d_notset", "EXISTS", "d")
    chk("partial_e_notset", "EXISTS", "e")
    chk("partial_a_unchanged", "GET", "a")
    # last pair's key exists -> still 0, none written
    chk("last_exists_reply", "MSETNX", "f", "6", "g", "7", "c", "8")
    chk("last_f_notset", "EXISTS", "f")
    chk("last_g_notset", "EXISTS", "g")
    # duplicate key within one call (x set, then x "exists" within the same op)
    flush()
    chk("dupkey_reply", "MSETNX", "x", "1", "x", "2")
    chk("dupkey_exists", "EXISTS", "x")
    chk("dupkey_val", "GET", "x")
    # SETNX
    flush()
    chk("setnx_new", "SETNX", "k", "v")
    chk("setnx_exists", "SETNX", "k", "v2")
    chk("setnx_val", "GET", "k")
    # odd-args arity errors
    chk("msetnx_oddargs", "MSETNX", "a", "1", "b")
    chk("mset_oddargs", "MSET", "a", "1", "b")
    chk("msetnx_noargs", "MSETNX", "a")
    # MSET always overwrites (contrast)
    for s in (od, fr):
        cmd(s, "SET", "m", "old")
    chk("mset_overwrite_reply", "MSET", "m", "new", "n", "2")
    chk("mset_overwrite_m", "GET", "m")
    chk("mset_overwrite_n", "GET", "n")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} MSETNX/SETNX divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — MSETNX atomicity + SETNX/MSET byte-exact vs redis 7.2.4 "
        "(all-or-nothing, dup-key, odd-args, overwrite contrast)"
    )


if __name__ == "__main__":
    main()
