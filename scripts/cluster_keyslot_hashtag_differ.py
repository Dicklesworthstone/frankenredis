#!/usr/bin/env python3
"""Differential gate for CLUSTER KEYSLOT {hash-tag} extraction, fr vs redis 7.2.4.

Redis cluster.c::keyHashSlot extracts the hash tag with exact boundary rules:
the substring between the FIRST '{' and the FIRST '}' that occurs AFTER it; if
there is no '{', no '}' after the '{', or the content between them is EMPTY, the
WHOLE key is hashed (CRC16 & 16383). These boundary cases are a classic source
of cluster-routing bugs (a wrong tag sends related keys to different slots).
cluster_admin_parity_gate.py pins 5 KEYSLOT cases; this exhaustively pins the
hash-tag extraction boundaries so any keyHashSlot regression is caught.

Usage: cluster_keyslot_hashtag_differ.py <oracle_port> <fr_port>
       Exit 0 = every KEYSLOT byte-exact, 1 = divergence.
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


# (key, note) — each must hash to the SAME slot on fr and redis.
KEYS = [
    (b"foo", "plain key, no tag"),
    (b"{user1000}.following", "simple tag"),
    (b"{user1000}.followers", "same tag -> same slot as .following"),
    (b"foo{bar}baz", "tag in the middle"),
    (b"{bar}", "tag is whole content"),
    (b"{tag1}{tag2}", "two tags -> uses the FIRST (tag1)"),
    (b"{}foo", "empty tag at start -> hash WHOLE key"),
    (b"foo{}", "empty tag at end -> hash WHOLE key"),
    (b"foo{}{bar}", "first {} empty -> hash WHOLE key (NOT bar)"),
    (b"{{bar}}zap", "nested -> tag is '{bar' (first } after first {)"),
    (b"}{abc}", "'}' before '{' -> tag is 'abc'"),
    (b"a{b", "no closing brace -> hash WHOLE key"),
    (b"a}b", "no opening brace -> hash WHOLE key"),
    (b"{", "lone '{' -> whole key"),
    (b"}", "lone '}' -> whole key"),
    (b"{}", "empty tag only -> whole key"),
    (b"", "empty key"),
    (b"\x00\xff\r\n", "binary key with CRLF"),
    (b"{\x00\xff}tail", "binary inside tag"),
    (b"hash{tag}with{second}", "second tag ignored, uses 'tag'"),
    (b"{a{b}c}", "tag is 'a{b' (first } closes)"),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = 0
    for key, note in KEYS:
        ro, rf = cmd(od, "CLUSTER", "KEYSLOT", key), cmd(fr, "CLUSTER", "KEYSLOT", key)
        if ro != rf:
            fails += 1
            print(f"DIFF KEYSLOT {key!r} ({note})\n  redis={ro!r}\n  fr   ={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {fails} CLUSTER KEYSLOT hash-tag divergence(s) vs redis 7.2.4")
        sys.exit(1)
    print(f"PASS — CLUSTER KEYSLOT hash-tag extraction byte-exact vs redis 7.2.4 ({len(KEYS)} keys)")


if __name__ == "__main__":
    main()
