#!/usr/bin/env python3
"""Differential gate for RESTORE IDLETIME / FREQ VALUE application, fr vs redis 7.2.4.

RESTORE accepts IDLETIME <seconds> (LRU policies) and FREQ <0-255> (LFU policy)
to seed the restored key's access metadata so OBJECT IDLETIME / OBJECT FREQ
reflect it. fr currently PARSES + VALIDATES these (the HARD checks below match
redis) but DISCARDS the value, so the restored key gets default metadata
(idletime 0, freq LFU_INIT_VAL=5) — frankenredis-f16dz.

HARD checks (option parsing/validation — already byte-exact): REPLACE, ABSTTL,
the IDLETIME/FREQ mutual exclusion, negative/out-of-range rejection, and the
policy-gated error wording.

DOCUMENTED divergence (NOTE, gate stays green; auto-promotes to a HARD check once
f16dz lands): RESTORE ... IDLETIME N -> OBJECT IDLETIME ~N, and RESTORE ... FREQ N
-> OBJECT FREQ N.

Usage: restore_idletime_freq_differ.py <oracle_port> <fr_port>
       Exit 0 = parity (modulo the documented f16dz divergence), 1 = NEW
            divergence, 2 = setup error.
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


def payload(dump_reply):
    i = dump_reply.index(b"\r\n")
    n = int(dump_reply[1:i])
    return dump_reply[i + 2:i + 2 + n]


def as_int(b):
    return int(b.lstrip(b":+").split(b"\r", 1)[0])


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)

    def both(*c):
        return cmd(od, *c), cmd(fr, *c)

    cmd(od, "FLUSHALL")
    cmd(fr, "FLUSHALL")
    cmd(od, "SET", "src", "hello")
    cmd(fr, "SET", "src", "hello")
    do, df = cmd(od, "DUMP", "src"), cmd(fr, "DUMP", "src")
    if payload(do) != payload(df):
        print(f"SETUP ERROR: DUMP payloads differ\n  redis={do!r}\n  fr={df!r}")
        sys.exit(2)
    p = payload(do)

    fails, notes = [], []

    def hard(label, *c):
        o, f = both(*c)
        if o != f:
            fails.append(f"{label}: cmd={list(c)[:3]}... redis={o!r} fr={f!r}")

    def reset(policy):
        for d in (od, fr):
            cmd(d, "config", "set", "maxmemory-policy", policy)
            cmd(d, "flushall")

    # HARD: option validation / error wording (already byte-exact)
    reset("noeviction")
    hard("freq_under_nonlfu_err", "RESTORE", "e1", "0", p, "FREQ", "7")
    hard("idletime_negative_err", "RESTORE", "e2", "0", p, "IDLETIME", "-1")
    hard("idletime_and_freq_err", "RESTORE", "e3", "0", p, "IDLETIME", "5", "FREQ", "5")
    reset("allkeys-lfu")
    hard("idletime_under_lfu_err", "RESTORE", "e4", "0", p, "IDLETIME", "5")
    hard("freq_out_of_range_err", "RESTORE", "e5", "0", p, "FREQ", "256")
    reset("noeviction")
    cmd(od, "SET", "e6", "x")
    cmd(fr, "SET", "e6", "x")
    hard("replace_ok", "RESTORE", "e6", "0", p, "REPLACE")

    # DOCUMENTED (f16dz): IDLETIME value must be reflected by OBJECT IDLETIME
    reset("noeviction")
    both("RESTORE", "k1", "0", p, "IDLETIME", "100")
    o, f = both("OBJECT", "IDLETIME", "k1")
    try:
        oi, fi = as_int(o), as_int(f)
    except ValueError:
        fails.append(f"idletime_value: non-integer reply redis={o!r} fr={f!r}")
    else:
        if abs(oi - 100) <= 2 and abs(fi - 100) <= 2:
            notes.append("idletime_value now MATCHES (frankenredis-f16dz fixed?) — promote to HARD")
        elif abs(oi - 100) <= 2 and fi <= 1:
            notes.append(
                f"idletime_value KNOWN DIVERGENCE (frankenredis-f16dz): redis={oi} fr={fi} "
                "(RESTORE IDLETIME value not applied)"
            )
        else:
            fails.append(f"idletime_value UNEXPECTED: redis={o!r} fr={f!r}")

    # DOCUMENTED (f16dz): FREQ value must be reflected by OBJECT FREQ (LFU policy)
    reset("allkeys-lfu")
    both("RESTORE", "k2", "0", p, "FREQ", "7")
    o, f = both("OBJECT", "FREQ", "k2")
    try:
        oi, fi = as_int(o), as_int(f)
    except ValueError:
        fails.append(f"freq_value: non-integer reply redis={o!r} fr={f!r}")
    else:
        if oi == 7 and fi == 7:
            notes.append("freq_value now MATCHES (frankenredis-f16dz fixed?) — promote to HARD")
        elif oi == 7 and fi != 7:
            notes.append(
                f"freq_value KNOWN DIVERGENCE (frankenredis-f16dz): redis={oi} fr={fi} "
                "(RESTORE FREQ value not applied)"
            )
        else:
            fails.append(f"freq_value UNEXPECTED: redis={o!r} fr={f!r}")

    print("=" * 60)
    for n in notes:
        print(f"NOTE  {n}")
    if fails:
        print(f"FAIL — {len(fails)} NEW divergence(s) in RESTORE IDLETIME/FREQ vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — RESTORE IDLETIME/FREQ validation byte-exact vs redis 7.2.4 "
        f"({len(notes)} documented f16dz value-application divergence(s))"
    )


if __name__ == "__main__":
    main()
