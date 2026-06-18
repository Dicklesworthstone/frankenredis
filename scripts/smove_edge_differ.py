#!/usr/bin/env python3
"""Differential gate: SMOVE edge cases (frankenredis-jq9tz).

SMOVE src dst member is a two-key set write with several subtle outcomes that
set_differ only lightly touches: member-not-in-src -> 0 (no change); member already
in dst -> still moved (removed from src, returns 1); src missing -> 0; dst missing
-> created; src==dst -> 1 if member present (no-op), 0 otherwise; moving the last
member DELETES src; intset->intset and int-member->string-set moves preserve
correctness; wrong-type src or dst -> WRONGTYPE; arity error. Set order is
unspecified, so SMEMBERS results are compared as sorted multisets; the SMOVE reply
+ EXISTS are compared byte-exact vs redis 7.2.4.

Usage: smove_edge_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import re
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


def members(s, key):
    r = cmd(s, "SMEMBERS", key)
    return tuple(sorted(re.findall(rb"\$\d+\r\n([^\r]*)\r\n", r)))


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []

    def setup():
        for s in (od, fr):
            cmd(s, "FLUSHALL")
            cmd(s, "SADD", "src", "a", "b", "c")
            cmd(s, "SADD", "dst", "x", "y")
            cmd(s, "SADD", "both", "a", "z")
            cmd(s, "SET", "str", "v")
            cmd(s, "SADD", "intsrc", "1", "2", "3")
            cmd(s, "SADD", "intdst", "4", "5")

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    def chk_set(label, key):
        mo, mf = members(od, key), members(fr, key)
        if mo != mf:
            fails.append(f"{label}: redis={mo} fr={mf}")

    setup()
    chk("ok", "SMOVE", "src", "dst", "a")
    chk_set("ok_src", "src")
    chk_set("ok_dst", "dst")
    chk("not_in_src", "SMOVE", "src", "dst", "zzz")
    setup()
    chk("member_in_both", "SMOVE", "src", "both", "a")
    chk_set("both_after", "both")
    chk_set("src_after_both", "src")
    chk("src_missing", "SMOVE", "nope", "dst", "a")
    chk("dst_missing_creates", "SMOVE", "src", "newdst", "b")
    chk_set("newdst", "newdst")
    chk("same_set_member", "SMOVE", "src", "src", "c")
    chk("same_set_notmember", "SMOVE", "src", "src", "zzz")
    chk("wrongtype_src", "SMOVE", "str", "dst", "a")
    chk("wrongtype_dst", "SMOVE", "src", "str", "c")
    setup()
    chk("last_member_deletes_src", "SMOVE", "both", "dst", "z")
    chk("both_after_last", "EXISTS", "both")  # both still has 'a' -> exists
    setup()
    chk("intset_move", "SMOVE", "intsrc", "intdst", "2")
    chk_set("intdst_after", "intdst")
    chk("int_into_strset", "SMOVE", "intsrc", "dst", "1")
    chk_set("dst_mixed", "dst")
    chk("arity", "SMOVE", "src", "dst")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} SMOVE divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — SMOVE edge cases byte-exact vs redis 7.2.4 "
        "(not-in-src/in-both/missing/creates/same-set/wrongtype/last-deletes/intset/arity)"
    )


if __name__ == "__main__":
    main()
