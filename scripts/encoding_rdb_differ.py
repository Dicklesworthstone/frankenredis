#!/usr/bin/env python3
"""encoding_rdb_differ.py — OBJECT ENCODING parity vs Redis 7.2.4 across the
config × RDB-path matrix. Consolidates the cc 2026-06-21 freeze-turn encoding
probes into one permanent gate. Covers hash/zset/set/list under non-default
entry-count and per-value caps, checking encoding after: live build+shrink,
RESTORE-of-dump, DEBUG RELOAD, and COPY.

All-must-pass strict gate (exit 1 on ANY diff). frankenredis-10ovx (list RESTORE
quicklist-stickiness downgrade) is FIXED (d516c8aa1); the earlier "DEBUG RELOAD"
divergence was a test artifact (fr DEBUG disabled). Run BOTH servers with
--enable-debug-command yes so the RELOAD checks are valid.

Usage: encoding_rdb_differ.py <redis_port> <fr_port>   (both running; redis needs
--enable-debug-command yes (BOTH servers) for the RELOAD checks).
"""
import socket, sys

class C:
    def __init__(s, p):
        s.s = socket.create_connection(("127.0.0.1", p), 5); s.s.settimeout(20); s.b = b""
    def _l(s):
        while b"\r\n" not in s.b: s.b += s.s.recv(65536)
        l, s.b = s.b.split(b"\r\n", 1); return l
    def read(s):
        l = s._l(); t = chr(l[0]); r = l[1:]
        if t in "+-:": return r
        if t == "$":
            n = int(r)
            if n < 0: return None
            while len(s.b) < n + 2: s.b += s.s.recv(65536)
            d = s.b[:n]; s.b = s.b[n + 2:]; return d
        if t == "*":
            n = int(r); return None if n < 0 else [s.read() for _ in range(n)]
        return r
    def cmd(s, *a):
        b = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            b += b"$%d\r\n%s\r\n" % (len(x), x)
        s.s.sendall(b); return s.read()

def main():
    red = C(int(sys.argv[1])); fr = C(int(sys.argv[2]))
    regressions = 0; known = 0; total = 0
    BIG = "x" * 100
    def enc(c, key): return c.cmd("OBJECT", "ENCODING", key)
    def check(label, key, is_known_10ovx=False):
        nonlocal regressions, known, total
        total += 1
        ra, fa = enc(red, key), enc(fr, key)
        if ra != fa:
            if is_known_10ovx:
                known += 1
                print(f"KNOWN-10ovx [{label}] redis={ra} fr={fa}")
            else:
                regressions += 1
                print(f"REGRESSION [{label}] redis={ra} fr={fa}")
    TYPES = [
        ("hash", "hash-max-listpack-entries", "hash-max-listpack-value",
         lambda c, i, v: c.cmd("HSET", "k", f"f{i}", v), lambda c, i: c.cmd("HDEL", "k", f"f{i}")),
        ("zset", "zset-max-listpack-entries", "zset-max-listpack-value",
         lambda c, i, v: c.cmd("ZADD", "k", str(i), v), lambda c, i: c.cmd("ZREM", "k", str(i))),
        ("set",  "set-max-listpack-entries",  "set-max-listpack-value",
         lambda c, i, v: c.cmd("SADD", "k", v), lambda c, i: c.cmd("SREM", "k", f"m{i}")),
    ]
    def both(*a):
        red.cmd(*a); fr.cmd(*a)
    # collections: entry-cap stickiness + value-cap + RDB paths
    for name, ecap, vcap, add, rem in TYPES:
        for cap in ("4", "128"):
            both("CONFIG", "SET", ecap, cap)
            if name == "set": both("CONFIG", "SET", "set-max-intset-entries", cap)
            both("CONFIG", "SET", vcap, "64")
            for n, shrink_to in ((10, 3), (200, 3)):
                both("FLUSHALL")
                for i in range(n): add(red, i, f"m{i}"); add(fr, i, f"m{i}")
                for i in range(n - shrink_to): rem(red, i); rem(fr, i)
                check(f"{name}_entrycap{cap}_n{n}_live", "k")
                da = red.cmd("DUMP", "k")
                if da is not None:
                    both("DEL", "r"); red.cmd("RESTORE", "r", "0", da); fr.cmd("RESTORE", "r", "0", da)
                    check(f"{name}_entrycap{cap}_n{n}_restore", "r")  # hash/zset/set RESTORE re-derives OK
                # DEBUG RELOAD must match (clean with fr --enable-debug-command yes).
                both("DEBUG", "RELOAD"); check(f"{name}_entrycap{cap}_n{n}_reload", "k")
        # per-value cap: one oversized element -> hashtable/skiplist
        both("CONFIG", "SET", ecap, "128"); both("CONFIG", "SET", vcap, "16")
        both("FLUSHALL")
        for i in range(5): v = BIG if i == 2 else f"v{i}"; add(red, i, v); add(fr, i, v)
        check(f"{name}_valcap_live", "k")
        da = red.cmd("DUMP", "k")
        if da is not None:
            both("DEL", "r"); red.cmd("RESTORE", "r", "0", da); fr.cmd("RESTORE", "r", "0", da)
            check(f"{name}_valcap_restore", "r")
    # lists: KNOWN-OPEN 10ovx on RESTORE + RELOAD; live + COPY must still match
    for cap in ("4", "128", "-2"):
        both("CONFIG", "SET", "list-max-listpack-size", cap)
        for n, shrink_to in ((10, 3), (130, 127), (400, 5)):
            both("FLUSHALL")
            for i in range(n): red.cmd("RPUSH", "k", f"e{i}"); fr.cmd("RPUSH", "k", f"e{i}")
            for i in range(n - shrink_to): red.cmd("RPOP", "k"); fr.cmd("RPOP", "k")
            check(f"list_cap{cap}_n{n}_live", "k")  # must match
            both("DEL", "c"); red.cmd("COPY", "k", "c"); fr.cmd("COPY", "k", "c")
            check(f"list_cap{cap}_n{n}_copy", "c")  # must match (COPY is clean)
            da = red.cmd("DUMP", "k")
            if da is not None:
                both("DEL", "r"); red.cmd("RESTORE", "r", "0", da); fr.cmd("RESTORE", "r", "0", da)
                check(f"list_cap{cap}_n{n}_restore", "r")  # 10ovx FIXED — must-pass, catches regressions
            # DEBUG RELOAD must match too (verified clean once fr is started with
            # --enable-debug-command yes; the earlier "reload divergence" was a test
            # artifact from fr DEBUG being disabled -> erroring no-op).
            both("DEBUG", "RELOAD"); check(f"list_cap{cap}_n{n}_reload", "k")
    print(f"\nTOTAL={total} REGRESSIONS={regressions} KNOWN-10ovx={known}")
    sys.exit(1 if regressions else 0)

if __name__ == "__main__":
    main()
