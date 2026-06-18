#!/usr/bin/env python3
"""Differential gate: encoding-threshold config respect, port-based (frankenredis-2moat).

fr must honor the per-type encoding-threshold config knobs when set to NON-default
values: a collection upgrades its encoding exactly when it crosses the configured
threshold (count or value-length). The existing encoding_config_boundary/lower gates
are self-orchestrating (spawn their own servers via --bin) and aren't in parity_suite;
this port-based gate runs against the shared pair so it can be CI-registered. It
CONFIG SETs each knob low, drives a collection across the boundary, asserts OBJECT
ENCODING matches redis 7.2.4, then restores the defaults (suite-safe).

(Contrast: frankenredis-uwhyl — fr ignores proto-max-bulk-len; the encoding knobs ARE
respected, as this gate confirms.)

Usage: encoding_config_respect_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

DEFAULTS = {
    "set-max-intset-entries": "512",
    "set-max-listpack-entries": "128",
    "set-max-listpack-value": "64",
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


def short(r):
    return r[: r.index(b"\r\n")] if b"\r\n" in r else r[:40]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []

    def setcfg(k, v):
        for s in (od, fr):
            cmd(s, "CONFIG", "SET", k, str(v))

    def each(*c):
        for s in (od, fr):
            cmd(s, *c)

    def enc(label, key):
        eo, ef = short(cmd(od, "OBJECT", "ENCODING", key)), short(cmd(fr, "OBJECT", "ENCODING", key))
        if eo != ef:
            fails.append(f"{label}: redis={eo!r} fr={ef!r}")

    each("FLUSHALL")
    for k, v in DEFAULTS.items():
        setcfg(k, v)

    # set-max-intset-entries = 4
    setcfg("set-max-intset-entries", 4)
    each("DEL", "si"); each("SADD", "si", "1", "2", "3", "4"); enc("intset_at", "si")
    each("SADD", "si", "5"); enc("intset_over", "si")
    # set-max-listpack-value
    setcfg("set-max-listpack-value", 8)
    each("DEL", "sv"); each("SADD", "sv", "short"); enc("set_shortval", "sv")
    each("SADD", "sv", "x" * 20); enc("set_longval_over", "sv")
    # hash entries + value
    setcfg("hash-max-listpack-entries", 3)
    each("DEL", "h"); each("HSET", "h", "a", "1", "b", "2", "c", "3"); enc("hash_at", "h")
    each("HSET", "h", "d", "4"); enc("hash_over", "h")
    setcfg("hash-max-listpack-value", 8)
    each("DEL", "hv"); each("HSET", "hv", "f", "short"); enc("hash_shortval", "hv")
    each("HSET", "hv", "g", "x" * 20); enc("hash_longval_over", "hv")
    # zset entries + value
    setcfg("zset-max-listpack-entries", 2)
    each("DEL", "z"); each("ZADD", "z", "1", "a", "2", "b"); enc("zset_at", "z")
    each("ZADD", "z", "3", "c"); enc("zset_over", "z")
    setcfg("zset-max-listpack-value", 8)
    each("DEL", "zv"); each("ZADD", "zv", "1", "short"); enc("zset_shortval", "zv")
    each("ZADD", "zv", "2", "x" * 20); enc("zset_longval_over", "zv")
    # list size
    setcfg("list-max-listpack-size", 3)
    each("DEL", "l"); each("RPUSH", "l", "a", "b", "c"); enc("list_at", "l")
    each("RPUSH", "l", "d"); enc("list_over", "l")

    # restore defaults (suite-safe)
    for k, v in DEFAULTS.items():
        setcfg(k, v)

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} encoding-config-respect divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — encoding-threshold config respect byte-exact vs redis 7.2.4 "
        "(set/hash/zset/list entries + values + intset, transitions at configured thresholds)"
    )


if __name__ == "__main__":
    main()
