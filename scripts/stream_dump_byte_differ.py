#!/usr/bin/env python3
"""Differential gate: stream DUMP byte-equality (frankenredis-aapu4).

Streams DUMP as RDB_TYPE_STREAM_LISTPACKS_3 (entry listpack macro-nodes + stream metadata).
A clean stream (no consumer groups, no deletions) is fully deterministic and must DUMP
byte-for-byte identical to redis 7.2.4 — this gate ASSERTS that across single-node and
multi-node (many-entry) shapes, plus RESTORE round-trip.

NOT byte-asserted (legitimately non-deterministic / known):
  - Consumer-group consumer seen/active-time + PEL delivery-time are wall-clock MS stamps
    that differ run-to-run, so a stream with a CG/PEL DUMPs different bytes on each server
    (timing, not a bug) — excluded.
  - REPORTED (frankenredis-aapu4): a stream with XDEL'd entries — redis RETAINS them as
    listpack tombstones (DUMP larger), fr COMPACTS them out (DUMP smaller). Data is
    identical (XLEN/XRANGE/RESTORE match); only the DUMP bytes diverge. Flip to asserted
    once aapu4 is fixed.

Usage: stream_dump_byte_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = clean-stream DUMP byte-exact, 1 = a NEW (non-aapu4) divergence.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=8)


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
    for s in (o, f):
        cmd(s, "FLUSHALL")
        # clean single-node stream (explicit deterministic IDs)
        for i in range(1, 11):
            cmd(s, "XADD", "s_small", f"{i}-0", "field", f"v{i}", "n", str(i))
        # multi-node stream (many entries -> multiple listpack macro-nodes)
        for i in range(1, 301):
            cmd(s, "XADD", "s_big", f"{i}-0", "f", "x" * 20)
        # explicit-last-id (XSETID beyond entries) — metadata, deterministic
        for i in range(1, 6):
            cmd(s, "XADD", "s_setid", f"{i}-0", "k", "v")
        cmd(s, "XSETID", "s_setid", "999-0")
        # the aapu4 case: deletions -> tombstones
        for i in range(1, 11):
            cmd(s, "XADD", "s_del", f"{i}-0", "k", "v")
        cmd(s, "XDEL", "s_del", "3-0", "7-0")

    fails, known = [], []

    def assert_exact(key):
        do, df = dump(o, key), dump(f, key)
        if do != df:
            fails.append(f"{key}: DUMP redis={len(do)}b fr={len(df)}b")
            return
        # RESTORE round-trip on both, compare XRANGE
        cmd(o, "RESTORE", key + "r", "0", do)
        cmd(f, "RESTORE", key + "r", "0", df)
        if cmd(o, "XRANGE", key + "r", "-", "+") != cmd(f, "XRANGE", key + "r", "-", "+"):
            fails.append(f"{key}: XRANGE after RESTORE differs")

    for key in ("s_small", "s_big", "s_setid"):
        assert_exact(key)
    # aapu4 reported case
    if dump(o, "s_del") != dump(f, "s_del"):
        known.append("s_del: XDEL tombstone retention (redis keeps, fr compacts)")
    # data must still match for the reported case
    if cmd(o, "XRANGE", "s_del", "-", "+") != cmd(f, "XRANGE", "s_del", "-", "+"):
        fails.append("s_del: live XRANGE differs (data!)")

    if known:
        print("KNOWN (frankenredis-aapu4, not asserted): " + "; ".join(known))
    if fails:
        print(f"FAIL — {len(fails)} NEW stream DUMP divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — clean stream DUMP/RESTORE byte-exact vs redis 7.2.4 "
          "(single-node, multi-node, explicit-last-id) [XDEL-tombstone reported as aapu4]")


if __name__ == "__main__":
    main()
