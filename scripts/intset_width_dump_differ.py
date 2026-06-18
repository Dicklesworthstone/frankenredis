#!/usr/bin/env python3
"""Differential gate: intset DUMP/RESTORE byte-equality across widths + sizes (frankenredis-acetq).

dump_byte_equality_gate has a single 5-element int16 intset; this pins the full intset
encode surface — INT16 / INT32 / INT64 width promotion, signed extremes, and large (but
still-intset, < set-max-intset-entries) sizes. That is the surface the acetq "set intset
canonical noalloc" perf lever rewrote (canonical sorted encode without a scratch alloc), so
this guards its byte-for-byte DUMP output + RESTORE round-trip + encoding against regression.

Asserts vs redis 7.2.4: DUMP byte-identical, OBJECT ENCODING == intset, SMEMBERS identical,
RESTORE round-trips to a byte-identical re-DUMP, across width/size shapes. Resets
set-max-intset-entries to the true default (512) first (config-pollution trap).

Usage: intset_width_dump_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

CASES = {
    "i16": ["1", "2", "3", "-5", "100", "32767", "-32768"],
    "i32": ["100000", "-100000", "2147483647", "-2147483648", "5", "0"],
    "i64": ["9223372036854775807", "-9223372036854775808", "42", "1000000000000", "-1"],
    "mixed_width": ["1", "100000", "9223372036854775807", "-1", "-100000", "-9223372036854775808"],
    "large": [str(i) for i in range(300)],          # >128, still intset (default cap 512)
    "neg_heavy": [str(-i) for i in range(150)],
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
        cmd(s, "CONFIG", "SET", "set-max-intset-entries", "512")

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
    # path uses a SEPARATE fr-persist encode_compact_set_intset. Exercise it via DEBUG RELOAD
    # on fr and assert each key's DUMP still matches redis. Conditional: skip if DEBUG disabled.
    if cmd(f, "DEBUG", "RELOAD").startswith(b"+OK"):
        for key in CASES:
            chk(f"reload_dump_{key}", dump(o, key), dump(f, key))

    if fails:
        print(f"FAIL — {len(fails)} intset DUMP divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — intset DUMP/RESTORE byte-exact vs redis 7.2.4 "
          "(int16/int32/int64 widths, signed extremes, large+neg-heavy) [guards acetq noalloc]")


if __name__ == "__main__":
    main()
