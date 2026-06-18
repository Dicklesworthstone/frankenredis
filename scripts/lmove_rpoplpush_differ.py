#!/usr/bin/env python3
"""Differential gate: LMOVE / RPOPLPUSH edge cases (frankenredis-pbi9l).

LMOVE src dst <LEFT|RIGHT> <LEFT|RIGHT> pops from one end of src and pushes to one
end of dst; RPOPLPUSH is the deprecated alias for `LMOVE src dst RIGHT LEFT`. The
subtle cases (only lightly touched by list_differ): all four direction combos, the
same-list rotation (src==dst, e.g. RIGHT->LEFT rotates), the RPOPLPUSH alias and its
self-rotate, moving the last element DELETES src, a missing src returns nil with no
change, a missing dst is created, wrong-type src/dst -> WRONGTYPE, bad direction and
arity errors. Lists are ordered, so LRANGE is compared byte-exact vs redis 7.2.4.

Usage: lmove_rpoplpush_differ.py <oracle_port> <fr_port>
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

    def setup():
        for s in (od, fr):
            cmd(s, "FLUSHALL")
            cmd(s, "RPUSH", "src", "a", "b", "c")
            cmd(s, "RPUSH", "dst", "x", "y")
            cmd(s, "RPUSH", "one", "solo")
            cmd(s, "SET", "str", "v")

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    def chk_list(label, key):
        ro, rf = cmd(od, "LRANGE", key, "0", "-1"), cmd(fr, "LRANGE", key, "0", "-1")
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    for d1, d2 in (("LEFT", "LEFT"), ("LEFT", "RIGHT"), ("RIGHT", "LEFT"), ("RIGHT", "RIGHT")):
        setup()
        chk(f"lmove_{d1}_{d2}", "LMOVE", "src", "dst", d1, d2)
        chk_list(f"lmove_{d1}_{d2}_src", "src")
        chk_list(f"lmove_{d1}_{d2}_dst", "dst")
    # same-list rotation
    setup(); chk("rotate_RL", "LMOVE", "src", "src", "RIGHT", "LEFT"); chk_list("rotate_RL_list", "src")
    setup(); chk("rotate_LR", "LMOVE", "src", "src", "LEFT", "RIGHT"); chk_list("rotate_LR_list", "src")
    # RPOPLPUSH alias
    setup(); chk("rpoplpush", "RPOPLPUSH", "src", "dst"); chk_list("rpoplpush_src", "src"); chk_list("rpoplpush_dst", "dst")
    setup(); chk("rpoplpush_self", "RPOPLPUSH", "src", "src"); chk_list("rpoplpush_self_list", "src")
    # last element deletes src
    setup(); chk("last", "LMOVE", "one", "dst", "LEFT", "RIGHT"); chk("last_src_gone", "EXISTS", "one")
    # missing src -> nil
    setup(); chk("missing_src", "LMOVE", "nope", "dst", "LEFT", "RIGHT"); chk("rpoplpush_missing", "RPOPLPUSH", "nope", "dst")
    # dst created
    setup(); chk("dst_creates", "LMOVE", "src", "newdst", "LEFT", "RIGHT"); chk_list("newdst", "newdst")
    # wrong type / bad direction / arity
    setup()
    chk("wrongtype_src", "LMOVE", "str", "dst", "LEFT", "RIGHT")
    chk("wrongtype_dst", "LMOVE", "src", "str", "LEFT", "RIGHT")
    chk("rpoplpush_wrongtype", "RPOPLPUSH", "str", "dst")
    chk("bad_dir", "LMOVE", "src", "dst", "UP", "DOWN")
    chk("arity", "LMOVE", "src", "dst", "LEFT")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} LMOVE/RPOPLPUSH divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — LMOVE/RPOPLPUSH edge cases byte-exact vs redis 7.2.4 "
        "(4 dirs/rotation/alias/last-deletes/missing/creates/wrongtype/bad-dir/arity)"
    )


if __name__ == "__main__":
    main()
