#!/usr/bin/env python3
"""Differential gate: RESTORE corrupt-payload error handling (frankenredis-v029r).

RESTORE deserializes an arbitrary client-supplied DUMP payload — a deserialization
attack surface where robust, byte-identical error handling ("Bad data format",
"DUMP payload version or checksum are wrong") matters and must never panic/UB. The
restore_idletime_freq gate covers the REPLACE/ABSTTL/IDLETIME/FREQ OPTION matrix but
NOT the corrupt-payload error cases. This gate DUMPs valid string/list/hash/zset/set
payloads, then RESTOREs them corrupted (flipped CRC byte, truncated, empty, garbage
version) and with bad args (negative/non-int TTL, bad option, negative IDLETIME),
asserting fr's error is byte-identical to redis 7.2.4 — plus a valid baseline.

Usage: restore_corrupt_payload_differ.py <oracle_port> <fr_port>
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


def dump_payload(s, key):
    r = cmd(s, "DUMP", key)
    nl = r.index(b"\r\n")
    return r[nl + 2 : nl + 2 + int(r[1:nl])]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "ss", "hello")
        cmd(s, "RPUSH", "ls", "a", "b", "c")
        cmd(s, "HSET", "hs", "f", "v", "g", "w")
        cmd(s, "ZADD", "zs", "1", "a", "2", "b")
        cmd(s, "SADD", "es", "x", "y", "z")
    # fr DUMP is byte-identical to redis (separately gated); use the oracle's payloads.
    pl = {k: dump_payload(od, k) for k in ("ss", "ls", "hs", "zs", "es")}
    fails = []
    n = 0

    def chk(label, *c):
        nonlocal n
        ro, rf = cmd(od, *c), cmd(fr, *c)
        n += 1
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    # valid baselines (each type round-trips)
    for t, p in pl.items():
        for s in (od, fr):
            cmd(s, "DEL", "d_" + t)
        chk(f"valid_{t}", "RESTORE", "d_" + t, "0", p)
    # corrupt CRC (flip the last byte of each payload)
    for t, p in pl.items():
        bad = p[:-1] + bytes([p[-1] ^ 0xFF])
        chk(f"bad_crc_{t}", "RESTORE", "c_" + t, "0", bad)
    # truncations
    chk("truncated_half", "RESTORE", "tr1", "0", pl["ls"][: len(pl["ls"]) // 2])
    chk("truncated_5", "RESTORE", "tr2", "0", pl["ss"][:5])
    chk("truncated_1", "RESTORE", "tr3", "0", pl["ss"][:1])
    chk("empty_payload", "RESTORE", "ep", "0", b"")
    # garbage / bad type byte / bad version
    chk("garbage", "RESTORE", "gb", "0", b"\xff\xff\xffgarbage\x00\x01")
    chk("bad_type_byte", "RESTORE", "bt", "0", b"\x7f" + pl["ss"][1:])  # unknown RDB type 0x7f
    chk("version_hi", "RESTORE", "vh", "0", pl["ss"][:-10] + b"\xff\xff" + pl["ss"][-8:])
    # bad args
    chk("neg_ttl", "RESTORE", "n1", "-1", pl["ss"])
    chk("notint_ttl", "RESTORE", "n2", "notanum", pl["ss"])
    chk("bad_opt", "RESTORE", "n3", "0", pl["ss"], "FOO")
    chk("neg_idletime", "RESTORE", "n4", "0", pl["ss"], "IDLETIME", "-5")
    chk("notint_idletime", "RESTORE", "n5", "0", pl["ss"], "IDLETIME", "x")
    # busykey (restore over an existing key without REPLACE)
    chk("busykey", "RESTORE", "d_ss", "0", pl["ss"])           # d_ss already restored above
    chk("busykey_replace_ok", "RESTORE", "d_ss", "0", pl["ss"], "REPLACE")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} RESTORE corrupt-payload divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — RESTORE corrupt-payload error handling byte-exact vs redis 7.2.4 "
        f"({n} cases: valid baselines + bad-crc/truncated/garbage/version + bad-args + busykey)"
    )


if __name__ == "__main__":
    main()
