#!/usr/bin/env python3
"""Differential gate: BITFIELD overflow-mode matrix (frankenredis-tfe33).

bitfield_differ fuzzes BITFIELD randomly; this pins the DETERMINISTIC overflow
semantics: OVERFLOW WRAP/SAT/FAIL applied to signed and unsigned over- and
under-flow, the #N (offset = N*width) notation, GET past the string end returning 0,
the type-width range (u1..u63 unsigned, i1..i64 signed) including invalid u64/i65,
the OVERFLOW directive's scope across multiple ops in one call, error cases (bad
type, bad overflow mode), and BITFIELD_RO rejecting write ops. Byte-exact vs redis
7.2.4.

Usage: bitfield_overflow_differ.py <oracle_port> <fr_port>
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

    def reset():
        for s in (od, fr):
            cmd(s, "DEL", "bf")

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    # unsigned overflow
    reset(); chk("u8_wrap", "BITFIELD", "bf", "SET", "u8", "0", "255", "OVERFLOW", "WRAP", "INCRBY", "u8", "0", "1")
    reset(); chk("u8_sat", "BITFIELD", "bf", "SET", "u8", "0", "255", "OVERFLOW", "SAT", "INCRBY", "u8", "0", "1")
    reset(); chk("u8_fail", "BITFIELD", "bf", "SET", "u8", "0", "255", "OVERFLOW", "FAIL", "INCRBY", "u8", "0", "1")
    # signed over/underflow
    reset(); chk("i8_wrap", "BITFIELD", "bf", "SET", "i8", "0", "127", "OVERFLOW", "WRAP", "INCRBY", "i8", "0", "1")
    reset(); chk("i8_sat", "BITFIELD", "bf", "SET", "i8", "0", "127", "OVERFLOW", "SAT", "INCRBY", "i8", "0", "1")
    reset(); chk("i8_fail", "BITFIELD", "bf", "SET", "i8", "0", "127", "OVERFLOW", "FAIL", "INCRBY", "i8", "0", "1")
    reset(); chk("i8_under_wrap", "BITFIELD", "bf", "SET", "i8", "0", "-128", "OVERFLOW", "WRAP", "INCRBY", "i8", "0", "-1")
    reset(); chk("i8_under_sat", "BITFIELD", "bf", "SET", "i8", "0", "-128", "OVERFLOW", "SAT", "DECRBY", "i8", "0", "1")
    # #N offset
    reset(); chk("hash_offset", "BITFIELD", "bf", "SET", "u8", "#0", "65", "SET", "u8", "#1", "66", "GET", "u8", "#0", "GET", "u8", "#1")
    # GET beyond end -> 0
    reset()
    for s in (od, fr):
        cmd(s, "SET", "bf", "A")
    chk("get_beyond", "BITFIELD", "bf", "GET", "u8", "100")
    chk("get_partial_beyond", "BITFIELD", "bf", "GET", "u16", "0")
    # widths
    reset(); chk("u63_max", "BITFIELD", "bf", "SET", "u63", "0", "9223372036854775807", "GET", "u63", "0")
    reset(); chk("i64_full", "BITFIELD", "bf", "SET", "i64", "0", "-1", "GET", "i64", "0")
    reset(); chk("u1", "BITFIELD", "bf", "SET", "u1", "0", "1", "GET", "u1", "0")
    # multi-op OVERFLOW scope
    reset(); chk("multi_overflow_scope", "BITFIELD", "bf", "SET", "u8", "0", "250", "OVERFLOW", "SAT", "INCRBY", "u8", "0", "10", "OVERFLOW", "WRAP", "INCRBY", "u8", "0", "10")
    # errors
    reset(); chk("bad_type", "BITFIELD", "bf", "GET", "x8", "0")
    chk("u64_invalid", "BITFIELD", "bf", "GET", "u64", "0")
    chk("i65_invalid", "BITFIELD", "bf", "GET", "i65", "0")
    chk("bad_overflow", "BITFIELD", "bf", "OVERFLOW", "BADMODE", "GET", "u8", "0")
    chk("set_val_overflow", "BITFIELD", "bf", "SET", "u8", "0", "256")
    # BITFIELD_RO
    reset()
    for s in (od, fr):
        cmd(s, "SET", "bf", "AB")
    chk("ro_get", "BITFIELD_RO", "bf", "GET", "u8", "0")
    chk("ro_rejects_set", "BITFIELD_RO", "bf", "SET", "u8", "0", "1")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} BITFIELD overflow divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — BITFIELD overflow-mode matrix byte-exact vs redis 7.2.4 "
        "(WRAP/SAT/FAIL signed+unsigned, #N offset, GET-beyond, widths, multi-op scope, errors, RO)"
    )


if __name__ == "__main__":
    main()
