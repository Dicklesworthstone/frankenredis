#!/usr/bin/env python3
"""Differential gate: GETEX TTL side-effects (frankenredis-xxlxx).

GETEX returns the value (like GET) but its real subtlety is the optional TTL
mutation: no option leaves the TTL untouched, PERSIST clears it, EX/PX set a
relative TTL, EXAT/PXAT set an absolute one, and conflicting / invalid options
error. This gate pins all of that byte-exact vs vendored redis 7.2.4 using only
DETERMINISTIC signals — the returned value, the absolute EXPIRETIME/PEXPIRETIME
(stable, unlike relative TTL which can tick a 1s boundary between the two servers),
PERSIST -> TTL -1, errors, and OBJECT ENCODING preservation.

Usage: getex_ttl_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

EXAT = "4102444800"        # 2100-01-01 in seconds (stable absolute)
EXAT2 = "4102444900"
PXAT = "4102444800000"


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

    def setup():
        for s in (od, fr):
            cmd(s, "FLUSHALL")
            cmd(s, "SET", "k", "val", "EXAT", EXAT)   # value + a stable absolute TTL
            cmd(s, "SET", "not", "v")                 # no TTL
            cmd(s, "SET", "ik", "12345")              # int-encoded
            cmd(s, "RPUSH", "lst", "a")

    # no-option GETEX: returns value, TTL untouched (absolute EXPIRETIME unchanged)
    setup()
    chk("noopt_value", "GETEX", "k")
    chk("noopt_keeps_exat", "EXPIRETIME", "k")
    # PERSIST: clears TTL (-> -1)
    setup()
    chk("persist_value", "GETEX", "k", "PERSIST")
    chk("persist_clears", "TTL", "k")
    chk("persist_on_nottl", "GETEX", "not", "PERSIST")
    chk("persist_nottl_stays", "TTL", "not")
    # EXAT / PXAT: set a new absolute TTL
    setup()
    chk("exat_value", "GETEX", "k", "EXAT", EXAT2)
    chk("exat_set", "EXPIRETIME", "k")
    setup()
    chk("pxat_value", "GETEX", "k", "PXAT", PXAT)
    chk("pxat_set", "PEXPIRETIME", "k")
    # missing key / wrong type
    setup()
    chk("missing", "GETEX", "nope")
    chk("missing_persist", "GETEX", "nope", "PERSIST")
    chk("wrongtype", "GETEX", "lst")
    # value + encoding preserved through a TTL-setting GETEX
    setup()
    chk("int_value", "GETEX", "ik", "EXAT", EXAT)
    chk("int_encoding", "OBJECT", "ENCODING", "ik")
    # error shapes
    setup()
    chk("err_persist_ex", "GETEX", "k", "EX", "100", "PERSIST")
    chk("err_ex_px", "GETEX", "k", "EX", "100", "PX", "1000")
    chk("err_ex_noarg", "GETEX", "k", "EX")
    chk("err_ex_zero", "GETEX", "k", "EX", "0")
    chk("err_ex_neg", "GETEX", "k", "EX", "-1")
    chk("err_bad_opt", "GETEX", "k", "FOO")
    chk("err_ex_notint", "GETEX", "k", "EX", "abc")
    chk("err_exat_zero", "GETEX", "k", "EXAT", "0")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} GETEX divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — GETEX TTL side-effects byte-exact vs redis 7.2.4 "
        "(value/PERSIST-clears/EXAT-PXAT-sets/no-opt-preserves/errors/encoding)"
    )


if __name__ == "__main__":
    main()
