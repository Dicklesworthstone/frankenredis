#!/usr/bin/env python3
"""Differential gate: list RDB round-trip across the encoding boundary (frankenredis-rzomz).

Lists serialize to RDB as a listpack (small) or a quicklist of listpack nodes
(large), crossing at list-max-listpack-size. fr reuses its packed list nodes when
writing RDB (frankenredis-x1mmu, bbc6daeaa) — a fast path that must still produce a
byte-identical DUMP payload, preserve OBJECT ENCODING, and survive DEBUG RELOAD with
an unchanged digest. This gate pins all of that vs vendored redis 7.2.4 across list
sizes spanning the listpack<->quicklist boundary (1..1000) and two element widths,
checking: DUMP bytes, OBJECT ENCODING (before + after RELOAD), DEBUG DIGEST-VALUE
after RELOAD, and full LRANGE after RELOAD.

Both servers are pinned to list-max-listpack-size=128 so the boundary is identical.

Usage: list_rdb_roundtrip_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact + digest-faithful round-trip, 1 = divergence.
"""
import socket
import sys
import time

SIZES = [1, 5, 64, 127, 128, 129, 200, 500, 1000]
ELEM_WIDTHS = [5, 40]


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.03)
    return s.recv(1 << 20)


def build(s, key, n, width):
    cmd(s, "DEL", key)
    els = [("e%d" % i).ljust(width, "x") for i in range(n)]
    for i in range(0, len(els), 50):
        cmd(s, "RPUSH", key, *els[i : i + 50])


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "CONFIG", "SET", "list-max-listpack-size", "128")
        cmd(s, "FLUSHALL")
    fails = []
    n_checks = 0
    for n in SIZES:
        for width in ELEM_WIDTHS:
            build(od, "l", n, width)
            build(fr, "l", n, width)
            n_checks += 1
            checks = {
                "DUMP": (cmd(od, "DUMP", "l"), cmd(fr, "DUMP", "l")),
                "ENCODING": (cmd(od, "OBJECT", "ENCODING", "l"), cmd(fr, "OBJECT", "ENCODING", "l")),
            }
            cmd(od, "DEBUG", "RELOAD")
            cmd(fr, "DEBUG", "RELOAD")
            checks["DIGEST_VALUE"] = (
                cmd(od, "DEBUG", "DIGEST-VALUE", "l"),
                cmd(fr, "DEBUG", "DIGEST-VALUE", "l"),
            )
            checks["ENCODING_RELOAD"] = (
                cmd(od, "OBJECT", "ENCODING", "l"),
                cmd(fr, "OBJECT", "ENCODING", "l"),
            )
            checks["LRANGE_RELOAD"] = (
                cmd(od, "LRANGE", "l", "0", "-1"),
                cmd(fr, "LRANGE", "l", "0", "-1"),
            )
            for what, (ro, rf) in checks.items():
                if ro != rf:
                    fails.append(f"n={n} width={width} {what}: redis={ro[:60]!r} fr={rf[:60]!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} list RDB round-trip divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — list RDB round-trip byte-exact vs redis 7.2.4 "
        f"({n_checks} list sizes x widths: DUMP + ENCODING + RELOAD digest/encoding/LRANGE, "
        "listpack<->quicklist boundary)"
    )


if __name__ == "__main__":
    main()
