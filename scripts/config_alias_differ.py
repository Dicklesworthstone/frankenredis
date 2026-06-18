#!/usr/bin/env python3
"""Differential gate: config aliases + negative list size, port-based (frankenredis-jd4kh).

redis keeps the deprecated ziplist-named encoding knobs (hash-max-ziplist-entries,
hash-max-ziplist-value, zset-max-ziplist-entries/value, list-max-ziplist-size) as
ALIASES of the listpack-named ones — a SET via either name updates the same value,
and GET via either returns it. list-max-listpack-size also accepts NEGATIVE values
(-1..-5 = size-based node limits: -2 = 8 KiB). This gate verifies fr's alias
reflection (both directions) and negative-size handling byte-exact vs redis 7.2.4
(full reply comparison, not just array shape), then restores defaults (suite-safe).

Usage: config_alias_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

LISTPACK_DEFAULTS = {
    "hash-max-listpack-entries": "128",
    "hash-max-listpack-value": "64",
    "zset-max-listpack-entries": "128",
    "zset-max-listpack-value": "64",
    "list-max-listpack-size": "128",
}


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

    def each(*c):
        for s in (od, fr):
            cmd(s, *c)

    def chk(label, *c):  # full reply comparison
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    cmd(od, "FLUSHALL"); cmd(fr, "FLUSHALL")
    for k, v in LISTPACK_DEFAULTS.items():
        each("CONFIG", "SET", k, v)

    # SET via ziplist alias -> reflected in listpack-named GET (both directions)
    each("CONFIG", "SET", "hash-max-ziplist-entries", "7")
    chk("alias_set_reflected", "CONFIG", "GET", "hash-max-listpack-entries")
    chk("alias_get_direct", "CONFIG", "GET", "hash-max-ziplist-entries")
    # a hash crossing the alias-set threshold upgrades encoding
    each("DEL", "h")
    each("HSET", "h", "a", "1", "b", "2", "c", "3", "d", "4", "e", "5", "f", "6", "g", "7", "h", "8")
    chk("hash_via_alias_threshold", "OBJECT", "ENCODING", "h")
    # SET via listpack name -> reflected in ziplist alias GET
    each("CONFIG", "SET", "zset-max-listpack-entries", "9")
    chk("listpack_set_ziplist_get", "CONFIG", "GET", "zset-max-ziplist-entries")
    # value aliases
    each("CONFIG", "SET", "hash-max-ziplist-value", "33")
    chk("value_alias_reflected", "CONFIG", "GET", "hash-max-listpack-value")
    chk("zset_value_alias", "CONFIG", "GET", "zset-max-ziplist-value")
    # negative list-max-listpack-size (size-based)
    chk("neg_list_set", "CONFIG", "SET", "list-max-listpack-size", "-2")
    chk("neg_list_get", "CONFIG", "GET", "list-max-listpack-size")
    chk("neg_list_alias_get", "CONFIG", "GET", "list-max-ziplist-size")
    each("DEL", "lbig")
    each("RPUSH", "lbig", "x" * 10000)  # one >8KiB entry -> quicklist under -2
    chk("neg_list_bigentry_enc", "OBJECT", "ENCODING", "lbig")

    # restore defaults (suite-safe)
    for k, v in LISTPACK_DEFAULTS.items():
        each("CONFIG", "SET", k, v)

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} config-alias divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — config ziplist-aliases + negative list size byte-exact vs redis 7.2.4 "
        "(alias reflection both ways + size-based node limit + threshold-driven encoding)"
    )


if __name__ == "__main__":
    main()
