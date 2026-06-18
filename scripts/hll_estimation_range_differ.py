#!/usr/bin/env python3
"""Differential gate: HLL estimation across cardinality ranges (frankenredis-2jr8a).

PFCOUNT switches estimators by cardinality — linear counting for small sets, the raw
HyperLogLog estimate in the middle, and a large-range correction above ~2.5*m
(m=16384 registers, so ~40960) — each with redis's exact bias tables. A bug in any
range threshold would diverge only at specific cardinalities. hll_core_differ checks
~2000; this walks the full range (1..50000, hitting the ~16384 and ~40960
boundaries), asserting PFCOUNT is byte-identical to redis 7.2.4 at each checkpoint,
plus dense-representation DUMP byte-equality, a PFMERGE union estimate, and a
multi-key PFCOUNT union. Estimates are deterministic for a fixed element set.

Usage: hll_estimation_range_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=10)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.003)
    return s.recv(1 << 20)


CHECKPOINTS = [1, 10, 100, 500, 1000, 2000, 5000, 12000, 16384, 20000, 40000, 50000]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "DEL", "hll")
    fails = []
    n = 0
    cur = 0
    for cp in CHECKPOINTS:
        while cur < cp:
            end = min(cur + 1000, cp)
            batch = [f"e{i}" for i in range(cur, end)]
            cmd(od, "PFADD", "hll", *batch)
            cmd(fr, "PFADD", "hll", *batch)
            cur = end
        co, cf = cmd(od, "PFCOUNT", "hll"), cmd(fr, "PFCOUNT", "hll")
        n += 1
        if co != cf:
            fails.append(f"card={cp}: redis={co!r} fr={cf!r}")
    # dense DUMP byte-equality at the largest cardinality
    do, df = cmd(od, "DUMP", "hll"), cmd(fr, "DUMP", "hll")
    n += 1
    if do != df:
        fails.append(f"dense_dump: redis_len={len(do)} fr_len={len(df)}")
    # PFMERGE union (disjoint 20k + 20k) + multi-key PFCOUNT
    for s in (od, fr):
        cmd(s, "DEL", "h1", "h2", "hm")
        for i in range(0, 20000, 1000):
            cmd(s, "PFADD", "h1", *[f"a{j}" for j in range(i, i + 1000)])
            cmd(s, "PFADD", "h2", *[f"b{j}" for j in range(i, i + 1000)])
        cmd(s, "PFMERGE", "hm", "h1", "h2")
    for label, args in (("merge_union", ("PFCOUNT", "hm")), ("multi_pfcount", ("PFCOUNT", "h1", "h2"))):
        ro, rf = cmd(od, *args), cmd(fr, *args)
        n += 1
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} HLL estimation divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — HLL PFCOUNT estimation byte-exact vs redis 7.2.4 across cardinality ranges "
        f"({n} checks: 1..50000 bias boundaries + dense DUMP + PFMERGE/multi-key union)"
    )


if __name__ == "__main__":
    main()
