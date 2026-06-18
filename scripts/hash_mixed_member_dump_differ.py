#!/usr/bin/env python3
"""Differential gate: hash listpack DUMP/RESTORE byte-equality (frankenredis-dv9n5).

dump_byte_equality_gate covers list/intset/zset listpacks but NO hash listpack case at all;
this pins the hash, including the MIXED case — a listpack-encoded hash whose fields and
values are a mix of integers and strings (so the listpack encodes some entries as LP
integers and some as LP strings). That is exactly the surface the dv9n5 "hash listpack
direct emit" perf lever rewrote, so this guards its byte-for-byte DUMP output + RESTORE
round-trip + encoding against any regression.

Asserts vs redis 7.2.4: DUMP byte-identical, OBJECT ENCODING == listpack, HGETALL identical,
RESTORE round-trips to a byte-identical re-DUMP, across several hash shapes. Resets the hash
listpack config to true defaults first (the documented oracle config-pollution trap).

Usage: hash_mixed_member_dump_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

# (key, HSET field/value args) — each stays < 128 entries so it remains listpack.
CASES = {
    "mh_small": ["f1", "100", "42", "alpha", "f3", "9223372036854775807", "beta", "v", "0", "x"],
    "mh_intheavy": sum(([str(i), str(i * 3)] for i in range(40)), []),
    "mh_strheavy": sum(([f"f{i}", f"val{i}"] for i in range(30)), []),
    "mh_mixedfv": sum(([(str(i) if i % 2 else f"k{i}"), (f"v{i}" if i % 3 else str(i * 9))]
                       for i in range(24)), []),
    "mh_negzero": ["-0", "a", "0", "b", "-1", "neg", "1", "pos"],
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
        for cfg in (("hash-max-listpack-entries", "128"), ("hash-max-listpack-value", "64")):
            cmd(s, "CONFIG", "SET", *cfg)

    def chk(label, ro, rf):
        if ro != rf:
            fails.append(f"{label}: redis={ro[:60]!r} fr={rf[:60]!r}")

    for key, args in CASES.items():
        for s in (o, f):
            cmd(s, "DEL", key)
            cmd(s, "HSET", key, *args)
        chk(f"dump_{key}", dump(o, key), dump(f, key))
        chk(f"enc_{key}", cmd(o, "OBJECT", "ENCODING", key), cmd(f, "OBJECT", "ENCODING", key))
        chk(f"hgetall_{key}", cmd(o, "HGETALL", key), cmd(f, "HGETALL", key))
        pl_o, pl_f = dump(o, key), dump(f, key)
        cmd(o, "RESTORE", key + "r", "0", pl_o)
        cmd(f, "RESTORE", key + "r", "0", pl_f)
        chk(f"redump_{key}", dump(o, key + "r"), dump(f, key + "r"))

    if fails:
        print(f"FAIL — {len(fails)} hash listpack DUMP divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — hash listpack DUMP/RESTORE byte-exact vs redis 7.2.4 "
          "(int+string fields/values, 5 shapes) [guards dv9n5 direct-emit]")


if __name__ == "__main__":
    main()
