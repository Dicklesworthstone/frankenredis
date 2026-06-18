#!/usr/bin/env python3
"""Differential gate: LCS edge & error cases (frankenredis-jz6k4).

algebra_resp3_differ already fuzzes LCS over random values (LEN/IDX/MINMATCHLEN/
WITHMATCHLEN), but a random fuzzer won't reliably hit the DETERMINISTIC edge and
error paths. This gate complements it by pinning exactly those byte-exact vs redis
7.2.4:
  * option errors: LEN+IDX together, MINMATCHLEN/WITHMATCHLEN without IDX, unknown
    option, non-integer MINMATCHLEN
  * structural edges: both/one empty string, identical strings, no common
    subsequence, scattered single-char matches, the same key on both sides,
    missing keys (treated as empty), and a payload past the 64-char bit-parallel
    word boundary
  * wrong type

Usage: lcs_edge_differ.py <oracle_port> <fr_port>
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

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    def pair(a, b):
        for s in (od, fr):
            cmd(s, "MSET", "k1", a, "k2", b)

    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "RPUSH", "lst", "x")  # wrong-type operand

    # option errors
    pair("a", "b")
    chk("err_len_idx", "LCS", "k1", "k2", "LEN", "IDX")
    chk("err_minmatch_no_idx", "LCS", "k1", "k2", "MINMATCHLEN", "4")
    chk("err_withml_no_idx", "LCS", "k1", "k2", "WITHMATCHLEN")
    chk("err_bad_opt", "LCS", "k1", "k2", "FOO")
    chk("err_minmatch_notint", "LCS", "k1", "k2", "IDX", "MINMATCHLEN", "abc")
    chk("err_wrongtype", "LCS", "lst", "k2")
    # both / one empty
    pair("", "")
    chk("both_empty", "LCS", "k1", "k2")
    chk("both_empty_len", "LCS", "k1", "k2", "LEN")
    chk("both_empty_idx", "LCS", "k1", "k2", "IDX")
    pair("abc", "")
    chk("one_empty", "LCS", "k1", "k2")
    chk("one_empty_idx", "LCS", "k1", "k2", "IDX", "WITHMATCHLEN")
    # identical / no-common / scattered
    pair("hello", "hello")
    chk("identical", "LCS", "k1", "k2")
    chk("identical_idx", "LCS", "k1", "k2", "IDX")
    pair("abc", "xyz")
    chk("no_common", "LCS", "k1", "k2")
    chk("no_common_len", "LCS", "k1", "k2", "LEN")
    chk("no_common_idx", "LCS", "k1", "k2", "IDX")
    pair("axbxc", "aybyc")
    chk("scattered_idx_wml", "LCS", "k1", "k2", "IDX", "WITHMATCHLEN")
    # the classic doc example + MINMATCHLEN edges
    pair("ohmytext", "mynewtext")
    chk("doc_basic", "LCS", "k1", "k2")
    chk("doc_idx_mm4", "LCS", "k1", "k2", "IDX", "MINMATCHLEN", "4")
    chk("doc_idx_mm0", "LCS", "k1", "k2", "IDX", "MINMATCHLEN", "0")
    chk("doc_idx_mm100", "LCS", "k1", "k2", "IDX", "MINMATCHLEN", "100")
    # same key twice
    chk("same_key_idx", "LCS", "k1", "k1", "IDX")
    # missing keys (treated as empty)
    for s in (od, fr):
        cmd(s, "DEL", "m1", "m2")
    chk("missing_both", "LCS", "m1", "m2")
    chk("missing_both_idx", "LCS", "m1", "m2", "IDX")
    for s in (od, fr):
        cmd(s, "SET", "m1", "abc")
    chk("missing_one", "LCS", "m1", "m2")
    # past the 64-char bit-parallel word boundary
    pair("a" * 80 + "XYZ" + "b" * 40, "c" * 30 + "XYZ" + "d" * 50)
    chk("long_basic", "LCS", "k1", "k2")
    chk("long_idx", "LCS", "k1", "k2", "IDX")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} LCS edge divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — LCS edge/error cases byte-exact vs redis 7.2.4 "
        "(option errors / empty / identical / no-common / missing / wrongtype / 64+ bit-parallel)"
    )


if __name__ == "__main__":
    main()
