#!/usr/bin/env python3
"""Differential gate: MIXED-member zset listpack DUMP/RESTORE byte-equality (frankenredis-vly2n).

dump_byte_equality_gate covers a string-member zset listpack; this pins the MIXED case —
a listpack-encoded zset whose members are a mix of integers and strings (so the listpack
encodes some entries as LP integers and some as LP strings), with mixed int/float scores.
That is exactly the surface the vly2n "mixed zset listpack direct emit" perf lever rewrote,
so this guards its byte-for-byte DUMP output + RESTORE round-trip + encoding against any
regression.

Asserts vs redis 7.2.4: DUMP byte-identical, OBJECT ENCODING == listpack, ZRANGE WITHSCORES
identical, RESTORE round-trips to a byte-identical re-DUMP, across several mixed shapes.
Resets zset/list listpack config to the true defaults first (the documented oracle
config-pollution trap).

Usage: zset_mixed_member_dump_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

# (key, ZADD score/member args) — each stays < 128 entries so it remains listpack.
CASES = {
    "mz_small": ["1", "100", "2.5", "alpha", "-3", "42", "0", "beta",
                 "9223372036854775807", "gamma", "3.14", "-99"],
    "mz_intheavy": sum(([str(i), (f"m{i}" if i % 3 == 0 else str(i * 7))] for i in range(40)), []),
    "mz_strheavy": sum(([str(i * 2), (str(i) if i % 4 == 0 else f"member-{i}")] for i in range(30)), []),
    "mz_negzero": ["-0", "a", "0", "b", "-1", "neg", "1", "pos"],
}


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=6)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.02)
    return s.recv(1 << 20)


def dump(s, k):
    r = cmd(s, "DUMP", k)
    if r[:1] != b"$":
        return r
    nl = r.index(b"\r\n")
    return r[nl + 2:nl + 2 + int(r[1:nl])]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    o, f = conn(op), conn(fp)
    fails = []
    for s in (o, f):
        cmd(s, "FLUSHALL")
        for cfg in (("zset-max-listpack-entries", "128"), ("zset-max-listpack-value", "64")):
            cmd(s, "CONFIG", "SET", *cfg)

    def chk(label, ro, rf):
        if ro != rf:
            fails.append(f"{label}: redis={ro[:60]!r} fr={rf[:60]!r}")

    for key, args in CASES.items():
        for s in (o, f):
            cmd(s, "DEL", key)
            cmd(s, "ZADD", key, *args)
        chk(f"dump_{key}", dump(o, key), dump(f, key))
        chk(f"enc_{key}", cmd(o, "OBJECT", "ENCODING", key), cmd(f, "OBJECT", "ENCODING", key))
        chk(f"zrange_{key}", cmd(o, "ZRANGE", key, "0", "-1", "WITHSCORES"),
            cmd(f, "ZRANGE", key, "0", "-1", "WITHSCORES"))
        # RESTORE round-trip: re-DUMP must be byte-identical on both
        pl_o, pl_f = dump(o, key), dump(f, key)
        cmd(o, "RESTORE", key + "r", "0", pl_o)
        cmd(f, "RESTORE", key + "r", "0", pl_f)
        chk(f"redump_{key}", dump(o, key + "r"), dump(f, key + "r"))

    if fails:
        print(f"FAIL — {len(fails)} mixed-member zset DUMP divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — mixed-member zset listpack DUMP/RESTORE byte-exact vs redis 7.2.4 "
          "(int+string members, mixed scores, 4 shapes) [guards vly2n direct-emit]")


if __name__ == "__main__":
    main()
