#!/usr/bin/env python3
"""Differential gate: version-ceiling parity (frankenredis-fwfb3).

fr targets redis 7.2.4. command_coverage_gate proves fr has all 7.2.4 commands;
this gate proves the INVERSE — fr has NOT drifted ahead by implementing post-7.2.4
commands. The redis 7.4 hash-field-TTL family (HEXPIRE/HPEXPIRE/HEXPIREAT/
HPEXPIREAT/HTTL/HPTTL/HEXPIRETIME/HPEXPIRETIME/HPERSIST/HGETEX/HGETDEL) and other
post-7.2.4 commands must produce the SAME unknown-command error on fr as on the
7.2.4 oracle. As a positive control, 7.0/7.2 commands that ARE in 7.2.4 (SINTERCARD/
EXPIRETIME/WAITAOF/CLIENT NO-TOUCH/BITFIELD_RO/SORT_RO/OBJECT FREQ) must work
identically. If a future change adds a 7.4 command to fr, this gate flags the drift.

Usage: version_ceiling_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence (fr drifted past 7.2.4).
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


# Must be UNKNOWN COMMAND on 7.2.4 (and therefore on fr).
POST_724 = [
    ["HEXPIRE", "h", "100", "FIELDS", "1", "f"],
    ["HPEXPIRE", "h", "100000", "FIELDS", "1", "f"],
    ["HEXPIREAT", "h", "99999999999", "FIELDS", "1", "f"],
    ["HPEXPIREAT", "h", "99999999999999", "FIELDS", "1", "f"],
    ["HTTL", "h", "FIELDS", "1", "f"],
    ["HPTTL", "h", "FIELDS", "1", "f"],
    ["HEXPIRETIME", "h", "FIELDS", "1", "f"],
    ["HPEXPIRETIME", "h", "FIELDS", "1", "f"],
    ["HPERSIST", "h", "FIELDS", "1", "f"],
    ["HGETEX", "h", "FIELDS", "1", "f"],
    ["HGETDEL", "h", "FIELDS", "1", "f"],
    ["FROBNICATE", "x", "y"],  # totally unknown baseline
]

# Positive control: present in 7.2.4, must work identically.
IN_724 = [
    ["SINTERCARD", "1", "s"],
    ["EXPIRETIME", "k"],
    ["WAITAOF", "0", "0", "0"],
    ["CLIENT", "NO-TOUCH", "ON"],
    ["BITFIELD_RO", "k", "GET", "u8", "0"],
    ["SORT_RO", "l", "ALPHA"],
    ["OBJECT", "FREQ", "k"],
    ["LMPOP", "1", "l", "LEFT"],
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "HSET", "h", "f", "v")
        cmd(s, "SET", "k", "v")
        cmd(s, "RPUSH", "l", "a")
        cmd(s, "SADD", "s", "m")
    fails = []
    for argv in POST_724 + IN_724:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{' '.join(argv)!r}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} version-ceiling divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — version-ceiling parity vs redis 7.2.4 "
        f"({len(POST_724)} post-7.2.4 commands reject identically, {len(IN_724)} in-7.2.4 work identically)"
    )


if __name__ == "__main__":
    main()
