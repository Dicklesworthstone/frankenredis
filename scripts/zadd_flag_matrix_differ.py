#!/usr/bin/env python3
"""Differential gate: ZADD flag-decision matrix (frankenredis-wfg4i).

ZADD's score-update decision is the canonical zset write and a classic bug source:
NX (only add-new), XX (only update-existing), GT (only if new>old), LT (only if
new<old), CH (count changed not just added), INCR (return new score / nil if a
condition blocks), and the conflict errors (NX+GT/LT, NX+XX, GT+LT). zset_differ
only lightly touches flags; this gate exhaustively crosses each flag-set with score
directions (equal/lower/higher), new-member adds, a multi-pair CH count, and the
conflicts — checking both the reply AND the resulting ZSCORE byte-exact vs redis
7.2.4.

Usage: zadd_flag_matrix_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

FLAGSETS = [
    [], ["NX"], ["XX"], ["GT"], ["LT"], ["CH"],
    ["GT", "CH"], ["LT", "CH"], ["NX", "CH"], ["XX", "CH"],
    ["GT", "XX"], ["LT", "XX"], ["GT", "XX", "CH"],
    ["NX", "GT"], ["NX", "XX"], ["GT", "LT"],   # last three are conflict errors
]
SCORES = ["5", "3", "7", "5"]  # equal, lower, higher, equal-again (member starts at 5)


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.015)
    return s.recv(1 << 20)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    n = 0

    def chk(label, *c):
        nonlocal n
        ro, rf = cmd(od, *c), cmd(fr, *c)
        n += 1
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    def reset():
        for s in (od, fr):
            cmd(s, "DEL", "z")
            cmd(s, "ZADD", "z", "5", "m")
            cmd(s, "ZADD", "z", "10", "other")

    # existing-member flag x score-direction matrix (reply + resulting score)
    for flags in FLAGSETS:
        for sc in SCORES:
            reset()
            tag = "_".join(flags) or "plain"
            chk(f"zadd_{tag}_{sc}", "ZADD", "z", *flags, sc, "m")
            chk(f"score_{tag}_{sc}", "ZSCORE", "z", "m")
    # adding a NEW member under each (non-conflict) flag
    for flags in (["NX"], ["XX"], ["GT"], ["LT"], ["CH"], ["GT", "CH"]):
        for s in (od, fr):
            cmd(s, "DEL", "z")
            cmd(s, "ZADD", "z", "5", "m")
        tag = "_".join(flags)
        chk(f"new_{tag}", "ZADD", "z", *flags, "8", "newm")
        chk(f"new_score_{tag}", "ZSCORE", "z", "newm")
    # multi-pair with GT+CH: a (3<5 no), b (9>5 yes), c (new) -> CH counts b + c = 2
    for s in (od, fr):
        cmd(s, "DEL", "z")
        cmd(s, "ZADD", "z", "5", "a", "5", "b")
    chk("multi_gt_ch", "ZADD", "z", "GT", "CH", "3", "a", "9", "b", "1", "c")
    chk("multi_a", "ZSCORE", "z", "a")
    chk("multi_b", "ZSCORE", "z", "b")
    chk("multi_c", "ZSCORE", "z", "c")
    # INCR forms (return new score / nil when a condition blocks)
    for flags, sc in ((["INCR"], "3"), (["NX", "INCR"], "3"), (["XX", "INCR"], "3"),
                      (["GT", "INCR"], "3"), (["GT", "INCR"], "-3"), (["LT", "INCR"], "-3")):
        reset()
        tag = "_".join(flags)
        chk(f"incr_{tag}_{sc}", "ZADD", "z", *flags, sc, "m")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} ZADD flag-matrix divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — ZADD flag-decision matrix byte-exact vs redis 7.2.4 "
        f"({n} checks: NX/XX/GT/LT/CH/INCR x score-direction + new-member + multi-pair CH + conflicts)"
    )


if __name__ == "__main__":
    main()
