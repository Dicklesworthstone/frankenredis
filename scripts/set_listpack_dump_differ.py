#!/usr/bin/env python3
"""Differential gate: set listpack DUMP/RESTORE byte-equality (frankenredis-tpans).

dump_byte_equality_gate has an intset set and a listpack zset but NO listpack-encoded SET
(a set with non-integer members, under set-max-listpack-* thresholds). This pins that case —
the surface the tpans "set listpack direct emit" perf lever rewrote — so its byte-for-byte
DUMP output + RESTORE round-trip + encoding (listpack, insertion-ordered) are guarded.

A listpack set is insertion-ordered, so DUMP bytes depend on member order; both engines get
identical SADD order, and byte-equal DUMP transitively proves member-order parity. Asserts
vs redis 7.2.4 across string / mixed int+string / binary / large (still-listpack) shapes.
Resets set-max-listpack config to true defaults first (config-pollution trap).

Usage: set_listpack_dump_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

CASES = {
    "sl_str": ["apple", "banana", "cherry", "date", "-5x", "007abc"],
    "sl_mixed": ["1", "2", "apple", "3", "banana"],          # int+str members => listpack, not intset
    "sl_binary": [b"a\x00b", b"c\xffd", "plain", b"\xfe\xfe"],
    "sl_large": [f"m{i}" for i in range(100)],               # 100 string members, still listpack
    "sl_longval": ["x" * 60, "y" * 60, "short"],             # values near the 64-byte cap
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
        for cfg in (("set-max-listpack-entries", "128"), ("set-max-listpack-value", "64")):
            cmd(s, "CONFIG", "SET", *cfg)

    def chk(label, ro, rf):
        if ro != rf:
            fails.append(f"{label}: redis={ro[:60]!r} fr={rf[:60]!r}")

    for key, args in CASES.items():
        for s in (o, f):
            cmd(s, "DEL", key)
            cmd(s, "SADD", key, *args)
        chk(f"dump_{key}", dump(o, key), dump(f, key))
        chk(f"enc_{key}", cmd(o, "OBJECT", "ENCODING", key), cmd(f, "OBJECT", "ENCODING", key))
        chk(f"smembers_{key}", cmd(o, "SMEMBERS", key), cmd(f, "SMEMBERS", key))
        pl_o, pl_f = dump(o, key), dump(f, key)
        cmd(o, "RESTORE", key + "r", "0", pl_o)
        cmd(f, "RESTORE", key + "r", "0", pl_f)
        chk(f"redump_{key}", dump(o, key + "r"), dump(f, key + "r"))

    # (RDB-save encoder coverage) DUMP exercises the fr-store command encoder; the RDB-save
    # path uses a SEPARATE fr-persist encode_compact_set_listpack. Exercise it via DEBUG RELOAD
    # on fr and assert each key's DUMP still matches redis. Conditional: skip if DEBUG disabled.
    if cmd(f, "DEBUG", "RELOAD").startswith(b"+OK"):
        for key in CASES:
            chk(f"reload_dump_{key}", dump(o, key), dump(f, key))

    if fails:
        print(f"FAIL — {len(fails)} set listpack DUMP divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — set listpack DUMP/RESTORE byte-exact vs redis 7.2.4 "
          "(string/mixed/binary/large/long-value, insertion order) [guards tpans direct-emit]")


if __name__ == "__main__":
    main()
