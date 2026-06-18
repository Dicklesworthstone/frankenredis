#!/usr/bin/env python3
"""Differential gate for the 2-key MGET / 2-pair MSET / multibulk PING fast-paths
(frankenredis-ohsk5), fr vs vendored redis 7.2.4.

These fast-paths special-case the exact packet shapes `MGET k1 k2`,
`MSET k1 v1 k2 v2`, and multibulk `PING [msg]` for throughput. A fast-path is a
correctness hazard: it must be byte-identical to the generic path on every edge
(a missing key, a wrong-type key, the SAME key twice, binary keys/values, TTL
clearing on MSET overwrite). This drives exactly those shapes with adversarial
inputs and compares replies + observable side effects (GET/TTL/TYPE) vs redis.

Usage: mget_mset_fastpath_differ.py <oracle_port> <fr_port>
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

    def both(*c):
        return cmd(od, *c), cmd(fr, *c)

    fails = []

    def check(label, *c):
        o, f = both(*c)
        if o != f:
            fails.append(f"{label} [{' '.join(map(str, c))[:48]}]: redis={o!r} fr={f!r}")

    def reset():
        cmd(od, "FLUSHALL")
        cmd(fr, "FLUSHALL")

    # --- 2-key MGET fast path ---
    reset()
    both("SET", "a", "1")
    both("SET", "b", "2")
    check("mget_both_present", "MGET", "a", "b")
    check("mget_one_missing", "MGET", "a", "nope")
    check("mget_both_missing", "MGET", "x", "y")
    check("mget_same_key_twice", "MGET", "a", "a")
    both("LPUSH", "lst", "z")
    check("mget_wrongtype_returns_nil", "MGET", "a", "lst")  # MGET never WRONGTYPEs
    check("mget_wrongtype_both", "MGET", "lst", "lst")
    both("SET", b"\x00\xff", b"\x01\x02\r\n")
    check("mget_binary_key", "MGET", b"\x00\xff", "a")
    # expired key reads as nil
    both("SET", "exp", "v")
    both("PEXPIRE", "exp", "1")
    time.sleep(0.02)
    check("mget_expired_nil", "MGET", "exp", "a")

    # --- 2-pair MSET fast path ---
    reset()
    check("mset_two_pairs", "MSET", "p1", "v1", "p2", "v2")
    check("mset_get_p1", "GET", "p1")
    check("mset_get_p2", "GET", "p2")
    # MSET overwrite must CLEAR an existing TTL on both keys
    both("SET", "t1", "old")
    both("EXPIRE", "t1", "1000")
    both("SET", "t2", "old")
    both("EXPIRE", "t2", "1000")
    check("mset_overwrite_ttl", "MSET", "t1", "new", "t2", "new")
    check("mset_ttl_cleared_t1", "TTL", "t1")
    check("mset_ttl_cleared_t2", "TTL", "t2")
    # binary values + same key twice in one MSET (last wins)
    check("mset_binary", "MSET", "bk", b"\x00\x01\x02", "bk2", b"\xff\xfe")
    check("mset_get_binary", "GET", "bk")
    check("mset_same_key_twice", "MSET", "dup", "first", "dup", "second")
    check("mset_dup_last_wins", "GET", "dup")
    # MSET replacing a wrong-type key
    both("LPUSH", "wl", "x")
    check("mset_over_wrongtype", "MSET", "wl", "now-a-string", "p3", "v3")
    check("mset_over_wrongtype_type", "TYPE", "wl")

    # --- multibulk PING fast path ---
    reset()
    check("ping_bare", "PING")
    check("ping_msg", "PING", "hello world")
    check("ping_binary", "PING", b"\x00\xff\r\n")
    check("ping_empty", "PING", "")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} fast-path divergence(s) vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — MGET/MSET/PING fast-path shapes byte-exact vs redis 7.2.4")


if __name__ == "__main__":
    main()
