#!/usr/bin/env python3
"""Differential gate: HyperLogLog core operations (frankenredis-cknkn).

Byte-exact HLL is hard: the sparse/dense register encoding, the PFADD change-detect
return, the cardinality ESTIMATE, and PFMERGE must all match redis 7.2.4 exactly
(DUMP of an HLL embeds the raw register string, so DUMP byte-equality directly
proves the encoding matches; the estimate is deterministic for a fixed element set).
This gate complements hll_corrupt_* (which test corruption handling) by pinning the
core ops + register-encoding byte-equality + the sparse->dense transition estimate.

Usage: hll_core_differ.py <oracle_port> <fr_port>
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
    fails = []

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    def both(*c):
        for s in (od, fr):
            cmd(s, *c)

    for s in (od, fr):
        cmd(s, "FLUSHALL")

    # PFADD change-detect + PFCOUNT exactness on small cardinalities
    chk("pfadd_new", "PFADD", "hll", "a", "b", "c")
    chk("pfcount_3", "PFCOUNT", "hll")
    chk("pfadd_dup", "PFADD", "hll", "a", "b")
    chk("pfadd_mix", "PFADD", "hll", "c", "d", "e")
    chk("pfcount_5", "PFCOUNT", "hll")
    chk("pfadd_noelem_existing", "PFADD", "hll")
    chk("pfadd_noelem_new", "PFADD", "hll2")
    chk("pfcount_empty", "PFCOUNT", "hll2")
    chk("pfcount_missing", "PFCOUNT", "nope")
    # sparse register encoding byte-equality
    chk("hll_dump_sparse", "DUMP", "hll")
    chk("hll_encoding", "OBJECT", "ENCODING", "hll")
    chk("hll_strlen_sparse", "STRLEN", "hll")
    # multi-key PFCOUNT union
    both("PFADD", "ha", "x", "y", "z")
    both("PFADD", "hb", "z", "w", "v")
    chk("pfcount_multi_union", "PFCOUNT", "ha", "hb")
    # PFMERGE -> dest + byte-equal dump
    both("DEL", "hm")
    chk("pfmerge", "PFMERGE", "hm", "ha", "hb")
    chk("pfmerge_count", "PFCOUNT", "hm")
    chk("pfmerge_dump", "DUMP", "hm")
    chk("pfmerge_into_existing", "PFMERGE", "ha", "hb")
    chk("pfmerge_existing_count", "PFCOUNT", "ha")
    both("DEL", "he")
    chk("pfmerge_no_source", "PFMERGE", "he")
    chk("pfmerge_no_source_count", "PFCOUNT", "he")
    # corrupt / wrong-type errors
    both("SET", "plain", "not-an-hll-value")
    chk("pfadd_corrupt", "PFADD", "plain", "x")
    chk("pfcount_corrupt", "PFCOUNT", "plain")
    chk("pfmerge_corrupt", "PFMERGE", "dst", "plain")
    both("DEL", "lst")
    both("RPUSH", "lst", "a")
    chk("pfadd_wrongtype", "PFADD", "lst", "x")
    # sparse->dense transition: estimate + dense register encoding byte-equality
    both("DEL", "big")
    for i in range(0, 2000, 100):
        both("PFADD", "big", *[f"e{j}" for j in range(i, i + 100)])
    chk("pfcount_2000_estimate", "PFCOUNT", "big")
    chk("hll_dump_dense", "DUMP", "big")
    chk("hll_strlen_dense", "STRLEN", "big")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} HLL core divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — HLL core byte-exact vs redis 7.2.4 "
        "(PFADD/PFCOUNT/PFMERGE + DUMP byte-equality sparse+dense + 2000-elem estimate)"
    )


if __name__ == "__main__":
    main()
