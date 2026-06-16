#!/usr/bin/env python3
"""zset_tiebreak_differ.py — adversarial zset ORDERING differ vs redis 7.2.4.

zset_differ.py fuzzes general zset behavior; this one specifically hammers the
ONE property a member-storage / index rewrite must never break: the sorted-set
total order under heavy EQUAL-score ties (where ordering falls to the member's
lexicographic byte order) plus binary members. It exists to guard the
Arc<[u8]>-shared member storage (frankenredis-peni2, c4417d55e) and the planned
structural-storage follow-up (frankenredis-uybhq): any change to how members are
stored, compared, or indexed in FullSortedSet (dict / ordered BTreeMap / rank
treap) must keep ZADD/ZINCRBY/ZREM/ZRANGE/ZRANK/ZREVRANK/ZRANGEBYLEX/
ZRANGEBYSCORE/ZPOPMIN + the final ZRANGE/ZRANGEBYLEX/DEBUG DIGEST-VALUE
byte-identical to redis.

The member pool is tiny and the score set is small with many repeats, so equal
scores are the common case and lex tie-breaking is exercised constantly; ~10 of
the members are random binary (incl. embedded high bytes) to catch any
content-vs-pointer comparison bug.

Usage: zset_tiebreak_differ.py <oracle_port> <fr_port> [seed] [iters]
Exit 0 if byte-exact, 1 on any divergence.
"""
import socket
import sys
import time
import random


def cli(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        if isinstance(x, str):
            x = x.encode()
        elif isinstance(x, int):
            x = str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    s.settimeout(2)
    d = b""
    try:
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            d += chunk
            if len(chunk) < 65536:
                break
    except socket.timeout:
        pass
    return d


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(1)
    O, F = int(sys.argv[1]), int(sys.argv[2])
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    iters = int(sys.argv[4]) if len(sys.argv) > 4 else 8000
    oc, fc = cli(O), cli(F)
    cmd(oc, "FLUSHALL")
    cmd(fc, "FLUSHALL")
    rng = random.Random(seed)
    pool = [f"m{i}".encode() for i in range(40)] + [
        bytes(rng.randint(0, 255) for _ in range(rng.randint(1, 4))) for _ in range(10)
    ]
    scores = ["0", "1", "-1", "1.5", "inf", "-inf", "2"]
    K = b"z"
    div = 0

    def both(*a):
        return cmd(oc, *a), cmd(fc, *a)

    for it in range(iters):
        op = rng.randint(0, 12)
        m = rng.choice(pool)
        sc = rng.choice(scores)
        if op <= 4:
            ro, rf = both("ZADD", K, sc, m)
        elif op == 5:
            ro, rf = both("ZINCRBY", K, "1", m)
        elif op == 6:
            ro, rf = both("ZREM", K, m)
        elif op == 7:
            a, b = sorted([rng.randint(-5, 45), rng.randint(-5, 45)])
            ro, rf = both("ZRANGE", K, a, b, "WITHSCORES")
        elif op == 8:
            ro, rf = both("ZRANK", K, m)
            ro2, rf2 = both("ZREVRANK", K, m)
            ro += ro2
            rf += rf2
        elif op == 9:
            ro, rf = both("ZRANGEBYLEX", K, "-", "+")
        elif op == 10:
            ro, rf = both("ZRANGEBYSCORE", K, "-inf", "+inf", "WITHSCORES")
        elif op == 11:
            ro, rf = both("ZPOPMIN", K, rng.randint(1, 3))
        else:
            ro, rf = both("ZSCORE", K, m)
            ro2, rf2 = both("ZCARD", K)
            ro += ro2
            rf += rf2
        if ro != rf:
            div += 1
            if div <= 5:
                print("DIVERGE it=%d op=%d m=%r sc=%s" % (it, op, m, sc))
                print("  oracle=%r" % ro)
                print("  fr    =%r" % rf)
    for probe in [
        ("ZRANGE", K, "0", "-1", "WITHSCORES"),
        ("ZRANGEBYLEX", K, "-", "+"),
        ("DEBUG", "DIGEST-VALUE", K),
    ]:
        ro, rf = both(*probe)
        if ro != rf:
            div += 1
            print("FINAL DIVERGE %r: oracle=%r fr=%r" % (probe, ro, rf))
    if div:
        print("\nFAIL: %d iters seed %d, divergences=%d" % (iters, seed, div))
        sys.exit(1)
    print("OK: %d iters seed %d, zset tie-break/order byte-exact vs redis 7.2.4" % (iters, seed))
    sys.exit(0)


if __name__ == "__main__":
    main()
