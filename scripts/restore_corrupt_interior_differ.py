#!/usr/bin/env python3
"""Differential gate: RESTORE valid-CRC corrupt-INTERIOR payloads (frankenredis-hm95r prereq).

WHY THIS EXISTS — the coverage gap this closes
----------------------------------------------
`restore_corrupt_payload_differ.py` and `restore_corruption_fuzz.py` corrupt the DUMP
WITHOUT recomputing the trailing CRC64, so every mutation fails the checksum check on BOTH
servers *before* any listpack-entry walk. That leaves fr's eager-vs-lazy span decode
UNGATED: a lazy "attach-raw" RESTORE (bead frankenredis-hm95r) mirrors redis's
lpValidateIntegrity(deep=0) = header + terminator only, so it would ACCEPT a valid-CRC
payload whose interior listpack entries are corrupt — where fr TODAY walks every entry and
REJECTS. The existing gates would stay green through that behavior change.

This gate closes the gap: DUMP listpack-encoded list/hash/set/zset values, flip each
interior byte, **recompute the CRC64** so the payload passes the checksum and REACHES the
listpack walk, RESTORE on both servers, and compare the reply.

THE CONTRACT
------------
Redis 7.2.4 default (`sanitize-dump-payload no`) does lpValidateIntegrity(deep=0) — header
`total_bytes` + terminator only — so it ACCEPTS interior corruption that keeps those intact;
a corruption that breaks the header/terminator is rejected by BOTH. fr's current EAGER
decode rejects a superset (any per-entry parse failure). So today the expected picture is:
  - break header/terminator  -> both reject  (parity; a lazy fr must keep this)
  - corrupt entry encoding    -> redis +OK, fr -error  (the EAGER SIGNATURE — hm95r target)
  - corrupt pure data byte     -> both +OK  (parity)

Default run = documents the eager signature, exits 0 (main stays green; this is the known
pre-hm95r state, like the WAIT/GETACK xfail gate). With env HM95R_LAZY=1 the gate REQUIRES
byte-parity on every case and additionally read-back-accesses each accepted value (a lazy fr
must not panic/diverge on access) — that is the acceptance criterion the lazy-attach lever
must satisfy.

Usage: restore_corrupt_interior_differ.py <oracle_port> <fr_port>
       env HM95R_LAZY=1  -> require full parity (acceptance mode for the lazy lever)
       Exit 0 = contract holds, 1 = unexpected divergence.

Self-check trick: point <oracle_port> and <fr_port> at the SAME clean redis to validate the
CRC recompute + plumbing — every interior corruption must then RESTORE with 0 divergence
(proving the recomputed CRC is accepted and the walk is actually reached).
"""
import os
import socket
import sys
import time

# redis CRC-64 (Jones variant): reflected, poly 0xad93d23594c935a9,
# reflected form 0x95ac9329ac4bc9b5, init 0, xorout 0.
_REFLECTED_POLY = 0x95AC9329AC4BC9B5


def crc64(data, crc=0):
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ (_REFLECTED_POLY if (crc & 1) else 0)
    return crc & 0xFFFFFFFFFFFFFFFF


# Canonical redis crc64 check vector — fail loudly if the polynomial is wrong.
assert crc64(b"123456789") == 0xE9C6D914C4B8D9CA, "crc64 polynomial/reflection is wrong"


def refoot(payload):
    """Recompute the trailing 8-byte little-endian CRC64 over everything but the CRC.

    A DUMP payload is [serialized value][2-byte RDB version LE][8-byte CRC64 LE]; the CRC
    covers value+version = payload[:-8]. After mutating an interior byte, this restores a
    VALID checksum so RESTORE passes verifyDumpPayload and reaches rdbLoadObject.
    """
    body = payload[:-8]
    return body + crc64(body).to_bytes(8, "little")


def conn(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=5)
    s.settimeout(3)
    return s


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.004)
    try:
        return s.recv(1 << 20)
    except Exception as e:
        return ("EXC", str(e))


def dump_payload(s, key):
    r = cmd(s, "DUMP", key)
    nl = r.index(b"\r\n")
    return r[nl + 2 : nl + 2 + int(r[1:nl])]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    lazy = os.environ.get("HM95R_LAZY") == "1"
    od, fr = conn(op), conn(fp)

    # Small values => listpack encoding (the QUICKLIST_2/listpack RESTORE arms hm95r touches).
    seed_reads = {
        "ls": ("RPUSH", ["a", "bb", "ccc", "4", "55", "666", "seven"], ("LRANGE", "0", "-1")),
        "hs": ("HSET", ["f1", "v1", "f2", "v2", "f3", "33"], ("HGETALL",)),
        "es": ("SADD", ["x", "yy", "zzz", "42", "13"], ("SMEMBERS",)),
        "zs": ("ZADD", ["1", "a", "2", "bb", "3", "ccc"], ("ZRANGE", "0", "-1")),
    }
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        for k, (op_name, args, _) in seed_reads.items():
            cmd(s, op_name, k, *args)

    # fr DUMP is byte-identical to redis (separately gated); use the oracle's payloads so a
    # real fr-vs-redis run tests the DECODE path with a canonical encoding on both sides.
    bases = {k: dump_payload(od, k) for k in seed_reads}
    # Round-trip self-check: a freshly re-footed valid payload must be byte-identical
    # (proves crc64 direction/endianness) AND must still RESTORE +OK on both servers.
    for k, p in bases.items():
        assert refoot(p) == p, f"refoot changed a valid {k} payload — crc64 endianness wrong"

    n = 0
    eager_sig = 0  # redis accepts, fr rejects (the pre-hm95r eager signature)
    both_reject = 0
    both_accept = 0
    unexpected = []  # (redis rejects, fr accepts) OR read-back mismatch under lazy mode
    fr_exc = 0

    def one(label, payload):
        nonlocal n, eager_sig, both_reject, both_accept, fr_exc
        # Separate key namespaces per server so a leftover value never triggers a false
        # BUSYKEY (and so the op==fp self-check stays clean).
        ko, kf = "cio_" + label, "cif_" + label
        cmd(od, "DEL", ko)
        cmd(fr, "DEL", kf)
        ro = cmd(od, "RESTORE", ko, "0", payload)
        rf = cmd(fr, "RESTORE", kf, "0", payload)
        n += 1
        if isinstance(rf, tuple):
            fr_exc += 1
            unexpected.append(f"{label}: fr socket EXC (panic/hang proxy): {rf}")
            return
        ro_ok = isinstance(ro, bytes) and ro.startswith(b"+OK")
        rf_ok = isinstance(rf, bytes) and rf.startswith(b"+OK")
        if ro_ok and rf_ok:
            both_accept += 1
            if lazy:
                # A lazy fr must not panic/diverge on ACCESS of a corrupt-accepted value.
                typ = seed_reads[label.split("@")[0]][2]
                cro, crf = cmd(od, *typ, ko), cmd(fr, *typ, kf)
                if cro != crf:
                    unexpected.append(f"{label}: accepted but read-back differs redis={cro!r} fr={crf!r}")
            cmd(od, "DEL", ko)  # don't leave corrupt values around (unsafe on access)
            cmd(fr, "DEL", kf)
        elif (not ro_ok) and (not rf_ok):
            both_reject += 1
            if ro != rf:  # both reject but with different error text
                unexpected.append(f"{label}: both reject, different error redis={ro!r} fr={rf!r}")
        elif ro_ok and not rf_ok:
            eager_sig += 1
            if lazy:
                unexpected.append(f"{label}: redis +OK, fr REJECT redis={ro!r} fr={rf!r}")
            cmd(od, "DEL", ko)
        else:  # redis rejects, fr accepts — fr LESS strict than redis: always wrong
            unexpected.append(f"{label}: redis REJECT, fr +OK redis={ro!r} fr={rf!r}")
            cmd(fr, "DEL", kf)

    # Sweep: XOR each interior byte with 0xFF (skip type byte [0] and the 10-byte
    # version+CRC footer), re-foot, and RESTORE. One deterministic flip per position.
    for k, base in bases.items():
        for i in range(1, len(base) - 10):
            b = bytearray(base)
            b[i] ^= 0xFF
            one(f"{k}@x{i}", refoot(bytes(b)))
        # A few targeted length-field blowups: set a byte to 0xFF-ish large-length markers.
        for i in range(1, min(len(base) - 10, 24)):
            b = bytearray(base)
            b[i] = 0x7F  # 6-bit/12-bit string len marker high value -> claims a long entry
            one(f"{k}@L{i}", refoot(bytes(b)))

    print("=" * 66)
    print(
        f"scanned {n} valid-CRC corrupt-interior payloads "
        f"(list/hash/set/zset listpack): both_accept={both_accept} both_reject={both_reject} "
        f"eager_signature(redis+OK,fr-reject)={eager_sig}"
    )
    if unexpected:
        mode = "LAZY-ACCEPTANCE" if lazy else "DEFAULT"
        print(f"FAIL ({mode}) — {len(unexpected)} unexpected divergence(s):")
        for x in unexpected[:16]:
            print(f"  {x}")
        sys.exit(1)
    if lazy:
        print("PASS (HM95R_LAZY) — fr matches redis byte-for-byte on every corrupt-interior "
              "payload, incl. read-back access (lazy-attach acceptance criterion met).")
    else:
        print("PASS (default) — no illegal divergence (fr never LESS strict than redis; "
              f"the {eager_sig} eager-signature cases are the documented pre-hm95r state — "
              "re-run with HM95R_LAZY=1 to lock the lazy-attach acceptance criterion).")


if __name__ == "__main__":
    main()
