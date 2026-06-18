#!/usr/bin/env python3
"""Differential gate: SET ... GET option matrix (frankenredis-0lqad).

The redis 7.0 GET option on SET returns the OLD value and interacts subtly with the
conditional/expiry options:
  * GET on a missing key -> nil (and sets); on an existing key -> old value (sets new)
  * NX GET on an existing key -> returns the old value but does NOT set (NX fails)
  * XX GET on a missing key -> nil and does NOT set
  * GET on a wrong-type key -> WRONGTYPE (and does not set)
  * EX/EXAT/KEEPTTL compose with GET; plain SET (no KEEPTTL) clears the TTL
  * NX+XX+GET -> syntax error
Pins all of it byte-exact vs redis 7.2.4 using deterministic signals only (returned
value, EXISTS/TYPE, absolute EXPIRETIME, TTL == -1 for cleared) so relative-TTL
timing never flakes. Also covers the legacy GETSET.

Usage: set_get_option_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

EXAT = "4102444800"   # 2100-01-01, stable absolute seconds


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

    def setk(v=None, ttl_exat=None, as_list=False):
        for s in (od, fr):
            cmd(s, "DEL", "k")
            if as_list:
                cmd(s, "RPUSH", "k", "listval")
            elif v is not None:
                if ttl_exat:
                    cmd(s, "SET", "k", v, "EXAT", ttl_exat)
                else:
                    cmd(s, "SET", "k", v)

    setk(); chk("get_missing", "SET", "k", "v1", "GET"); chk("get_missing_after", "GET", "k")
    setk("old"); chk("get_existing", "SET", "k", "new", "GET"); chk("get_existing_after", "GET", "k")
    setk("old"); chk("nx_get_exists", "SET", "k", "new", "NX", "GET"); chk("nx_get_exists_after", "GET", "k")
    setk(); chk("nx_get_missing", "SET", "k", "new", "NX", "GET"); chk("nx_get_missing_after", "GET", "k")
    setk(); chk("xx_get_missing", "SET", "k", "new", "XX", "GET"); chk("xx_get_missing_exists", "EXISTS", "k")
    setk("old"); chk("xx_get_exists", "SET", "k", "new", "XX", "GET"); chk("xx_get_exists_after", "GET", "k")
    setk(as_list=True); chk("get_wrongtype", "SET", "k", "v", "GET"); chk("get_wrongtype_type", "TYPE", "k")
    # EXAT + GET: returns old + sets the absolute expire
    setk("old"); chk("exat_get", "SET", "k", "new", "EXAT", EXAT, "GET"); chk("exat_get_time", "EXPIRETIME", "k")
    chk("exat_get_val", "GET", "k")
    # KEEPTTL + GET preserves the (absolute) expire; plain SET clears it
    setk("old", ttl_exat=EXAT); chk("keepttl_get", "SET", "k", "new", "KEEPTTL", "GET")
    chk("keepttl_get_time", "EXPIRETIME", "k")
    setk("old", ttl_exat=EXAT); chk("nokeepttl_get", "SET", "k", "new", "GET"); chk("nokeepttl_cleared", "TTL", "k")
    # int encoding preserved through GET
    setk("100"); chk("get_int", "SET", "k", "200", "GET"); chk("get_int_enc", "OBJECT", "ENCODING", "k")
    # conflicting options
    setk("old"); chk("nx_xx_get_err", "SET", "k", "v", "NX", "XX", "GET")
    # legacy GETSET
    setk("old"); chk("getset", "GETSET", "k", "new"); chk("getset_after", "GET", "k")
    setk(); chk("getset_missing", "GETSET", "k", "new")
    setk(as_list=True); chk("getset_wrongtype", "GETSET", "k", "v")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} SET-GET divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — SET ... GET option matrix byte-exact vs redis 7.2.4 "
        "(GET/NX/XX/EXAT/KEEPTTL/wrongtype/conflict + GETSET)"
    )


if __name__ == "__main__":
    main()
