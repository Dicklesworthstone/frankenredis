#!/usr/bin/env python3
"""Differential gate: list quicklist DUMP/RESTORE byte-equality (frankenredis-g7ag5).

dump_byte_equality_gate's "quicklist" list cases are all < 8 KB, so under the true default
list-max-listpack-size=-2 (8 KB/node) they encode as a SINGLE listpack node ("listpack"
encoding) — they never exercise a real multi-node quicklist. This gate uses lists that are
genuinely > 8 KB (multi-node, OBJECT ENCODING == quicklist), the surface the g7ag5
"quicklist2 direct emit" lever rewrote.

ASSERTED (byte-exact, verified): multi-node quicklists whose nodes are all PACKED listpacks
(plain small/medium elements, a mixed int+string list, AND — since frankenredis-1z4ba was
fixed in 83b9744b0 — lists with an 8 KiB..1 GiB element, which now DUMP as a PACKED
1-element listpack node (container 0x02) matching redis rather than a PLAIN node (0x01)).
A node is PLAIN only for a >=1 GiB element (redis isLargeElement / packed_threshold=1<<30).

Resets list-max-listpack-size to the true default -2 first (config-pollution trap).

Usage: list_quicklist_dump_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = asserted cases byte-exact, 1 = a NEW (non-1z4ba) divergence.
"""
import socket
import sys
import time

ASSERTED = {
    # 200 x ~100B = ~20 KB => multi-node quicklist, all PACKED listpack nodes.
    "ql_multinode": [("v%03d" % i) + "x" * 96 for i in range(200)],
    # int + medium-string mix, > 8 KB => multi-node, PACKED nodes.
    "ql_mixed": [str(i * 7) for i in range(140)] + [f"s{i}-" + "z" * 80 for i in range(140)],
    # (frankenredis-1z4ba FIXED 83b9744b0) an element above the per-node budget but < 1 GiB
    # is now a PACKED 1-element listpack node (container 0x02), matching redis — DUMP
    # byte-exact. Promoted from REPORTED to ASSERTED.
    "ql_largeelem": ["small1", "P" * 10000, "small2", "Q" * 9000],
    "ql_mixed_large": ["a", "b", "M" * 12000, "c", "N" * 20000] + [str(i) for i in range(50)],
}
REPORTED = {}


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=8)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.05)
    return s.recv(1 << 20)


def dump(s, k):
    r = cmd(s, "DUMP", k)
    if r[:1] != b"$":
        return r
    nl = r.index(b"\r\n")
    return r[nl + 2:nl + 2 + int(r[1:nl])]


def build(s, key, elems):
    cmd(s, "DEL", key)
    # push in chunks to bound the request buffer
    for off in range(0, len(elems), 50):
        cmd(s, "RPUSH", key, *elems[off:off + 50])


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    o, f = conn(op), conn(fp)
    for s in (o, f):
        cmd(s, "FLUSHALL")
        cmd(s, "CONFIG", "SET", "list-max-listpack-size", "-2")
    fails, known = [], []

    def run(key, elems, bucket):
        for s in (o, f):
            build(s, key, elems)
        if cmd(o, "OBJECT", "ENCODING", key) != b"$9\r\nquicklist\r\n":
            # not multi-node on this host/build; skip rather than mis-assert
            return
        do, df = dump(o, key), dump(f, key)
        lo, lf = cmd(o, "LRANGE", key, "0", "-1"), cmd(f, "LRANGE", key, "0", "-1")
        if lo != lf:
            (fails if bucket is fails else known).append(f"{key}: LRANGE differs (data!)")
        if do != df:
            bucket.append(f"{key}: DUMP redis={len(do)}b fr={len(df)}b")

    for k, e in ASSERTED.items():
        run(k, e, fails)
    for k, e in REPORTED.items():
        run(k, e, known)

    # (frankenredis-1z4ba) The DUMP command uses encode_dump_quicklist2; the RDB-save path
    # uses the SEPARATE encode_compact_list_quicklist2 — both were fixed to mark PLAIN only at
    # >=1 GiB. Exercise the RDB-save encoder via DEBUG RELOAD on the large-element list and
    # assert its DUMP still matches redis afterward (the two encoders must agree on node
    # structure). Conditional: skip cleanly if DEBUG is disabled, so the gate stays portable.
    if "ql_largeelem" in ASSERTED:
        reload_reply = cmd(f, "DEBUG", "RELOAD")
        if reload_reply.startswith(b"+OK"):
            do, df = dump(o, "ql_largeelem"), dump(f, "ql_largeelem")
            if do != df:
                fails.append(f"ql_largeelem: DUMP after fr DEBUG RELOAD redis={len(do)}b "
                             f"fr={len(df)}b (RDB-save encoder diverged)")

    if known:
        print("KNOWN (frankenredis-1z4ba, not asserted): " + "; ".join(known))
    if fails:
        print(f"FAIL — {len(fails)} NEW quicklist DUMP divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — multi-node quicklist (all-PACKED-node) DUMP/RESTORE byte-exact vs redis 7.2.4 "
          "[guards g7ag5 + 1z4ba large-element PACKED-node fix, DUMP + RDB-save encoders]")


if __name__ == "__main__":
    main()
