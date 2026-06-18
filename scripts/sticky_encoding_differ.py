#!/usr/bin/env python3
"""Differential gate: sticky-encoding / no-downgrade-on-shrink invariant (frankenredis-jpodb).

redis upgrades a collection's encoding when it crosses a size/value threshold and
NEVER downgrades it again, even after the collection shrinks back below the
threshold — once a hash is `hashtable`, a set `hashtable`, a zset `skiplist`, a list
`quicklist`, or an intset has become `listpack`, it stays there. An implementation
that re-compacted on shrink would diverge (and break DUMP/encoding parity). Most
encoding gates probe the UPGRADE boundary; this one pins the no-downgrade side.
Self-heals thresholds to defaults first, so it's immune to prior CONFIG SET.

Usage: sticky_encoding_differ.py <oracle_port> <fr_port>
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
    time.sleep(0.01)
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

    def each(*c):
        for s in (od, fr):
            cmd(s, *c)

    each("FLUSHALL")
    for k, v in [
        ("hash-max-listpack-entries", "128"),
        ("hash-max-listpack-value", "64"),
        ("set-max-listpack-entries", "128"),
        ("set-max-intset-entries", "512"),
        ("zset-max-listpack-entries", "128"),
        ("list-max-listpack-size", "128"),
    ]:
        each("CONFIG", "SET", k, v)

    # HASH: grow past 128 -> hashtable, shrink to 5 -> stays hashtable
    for i in range(200):
        each("HSET", "h", f"f{i}", "v")
    chk("hash_grown", "OBJECT", "ENCODING", "h")
    for i in range(5, 200):
        each("HDEL", "h", f"f{i}")
    chk("hash_shrunk", "OBJECT", "ENCODING", "h")
    chk("hash_shrunk_len", "HLEN", "h")
    # HASH big-value trigger -> hashtable, remove the big field -> stays hashtable
    each("DEL", "h2")
    each("HSET", "h2", "f", "x" * 100)
    each("HSET", "h2", "g", "y")
    chk("hash_bigval", "OBJECT", "ENCODING", "h2")
    each("HDEL", "h2", "f")
    chk("hash_bigval_removed", "OBJECT", "ENCODING", "h2")
    # SET listpack -> hashtable, shrink -> stays
    each("DEL", "st")
    for i in range(200):
        each("SADD", "st", f"m{i}")
    chk("set_grown", "OBJECT", "ENCODING", "st")
    for i in range(5, 200):
        each("SREM", "st", f"m{i}")
    chk("set_shrunk", "OBJECT", "ENCODING", "st")
    # intset grown past 512 -> hashtable, shrink -> stays
    each("DEL", "si")
    for i in range(600):
        each("SADD", "si", str(i))
    chk("intset_grown", "OBJECT", "ENCODING", "si")
    for i in range(10, 600):
        each("SREM", "si", str(i))
    chk("intset_shrunk", "OBJECT", "ENCODING", "si")
    # small intset -> add non-int -> listpack -> remove non-int -> stays listpack
    each("DEL", "si2")
    each("SADD", "si2", "1", "2", "3")
    chk("intset_small", "OBJECT", "ENCODING", "si2")
    each("SADD", "si2", "notanint")
    chk("intset_to_listpack", "OBJECT", "ENCODING", "si2")
    each("SREM", "si2", "notanint")
    chk("intset_no_downgrade", "OBJECT", "ENCODING", "si2")
    # ZSET grow -> skiplist, shrink -> stays
    each("DEL", "z")
    for i in range(200):
        each("ZADD", "z", str(i), f"m{i}")
    chk("zset_grown", "OBJECT", "ENCODING", "z")
    for i in range(5, 200):
        each("ZREM", "z", f"m{i}")
    chk("zset_shrunk", "OBJECT", "ENCODING", "z")
    # LIST grow -> quicklist, shrink -> stays
    each("DEL", "l")
    for i in range(200):
        each("RPUSH", "l", f"e{i}")
    chk("list_grown", "OBJECT", "ENCODING", "l")
    for _ in range(195):
        each("LPOP", "l")
    chk("list_shrunk", "OBJECT", "ENCODING", "l")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} sticky-encoding divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — sticky-encoding (no downgrade on shrink) byte-exact vs redis 7.2.4 "
        "(hash/set/intset/zset/list keep upgraded encoding after shrinking)"
    )


if __name__ == "__main__":
    main()
