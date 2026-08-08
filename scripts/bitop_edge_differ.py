#!/usr/bin/env python3
"""Differential gate: BITOP edge cases (frankenredis-rbtmn).

bitmap_differ covers basic BITOP AND/OR/XOR/NOT; this complements it with the
bug-prone edges: operands of DIFFERENT lengths are zero-padded to the longest;
NOT takes EXACTLY one source key (more -> error); a missing source counts as an
all-zero empty string; an empty result DELETES a pre-existing destination (returns
0); the dest is overwritten regardless of its prior type; an unknown op is a syntax
error; a wrong-type source errors. Each case checks the reply + dest GET/EXISTS/TYPE
byte-exact vs redis 7.2.4.

Usage: bitop_edge_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import sys

from _respread import assert_ok, assert_seed, cmd, conn

# (label, argv, follow-up checks on the dest key)
STEPS = [
    "RESET",
    ("and", ["BITOP", "AND", "d", "a", "b"]), ("and_get", ["GET", "d"]), ("and_len", ["STRLEN", "d"]),
    ("or", ["BITOP", "OR", "d", "a", "b"]), ("or_get", ["GET", "d"]),
    ("xor", ["BITOP", "XOR", "d", "a", "b"]), ("xor_get", ["GET", "d"]),
    ("not", ["BITOP", "NOT", "d", "a"]), ("not_get", ["GET", "d"]),
    ("and_difflen", ["BITOP", "AND", "d", "a", "big"]), ("and_difflen_len", ["STRLEN", "d"]),
    ("not_multi_err", ["BITOP", "NOT", "d", "a", "b"]),
    ("and_missing", ["BITOP", "AND", "d", "a", "nope"]), ("and_missing_get", ["GET", "d"]),
    ("or_missing", ["BITOP", "OR", "d", "a", "nope"]), ("or_missing_get", ["GET", "d"]),
    ("and_all_missing", ["BITOP", "AND", "d", "none1", "none2"]), ("and_all_missing_exists", ["EXISTS", "d"]),
    ("not_missing", ["BITOP", "NOT", "d", "nope"]), ("not_missing_exists", ["EXISTS", "d"]),
    ("wrongtype", ["BITOP", "AND", "d", "a", "lst"]),
    ("bad_op", ["BITOP", "NAND", "d", "a", "b"]),
    ("single", ["BITOP", "AND", "d", "a"]), ("single_get", ["GET", "d"]),
    ("xor_self", ["BITOP", "XOR", "d", "a", "a"]), ("xor_self_get", ["GET", "d"]),
    "RESET",
    ("overwrite_type", ["BITOP", "AND", "dst", "a", "b"]), ("overwrite_type_type", ["TYPE", "dst"]),
]


def reset(s):
    assert_ok(cmd(s, "FLUSHALL"), "FLUSHALL")
    assert_ok(cmd(s, "SET", "a", "abc"), "SET a")              # 3 bytes
    assert_ok(cmd(s, "SET", "b", "ABCD"), "SET b")             # 4 bytes (different length)
    assert_ok(cmd(s, "SET", "big", "xxxxxxxxxx"), "SET big")   # 10 bytes
    assert_ok(cmd(s, "SET", "dst", "preexisting"), "SET dst")  # dest exists as a string
    assert_seed(cmd(s, "RPUSH", "lst", "x"), 1, "RPUSH lst")   # wrong-type source


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    n = 0
    for step in STEPS:
        if step == "RESET":
            reset(od)
            reset(fr)
            continue
        label, argv = step
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        n += 1
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} BITOP edge divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — BITOP edge cases byte-exact vs redis 7.2.4 "
        f"({n} checks: NOT-arity/diff-len-pad/missing-as-zero/empty-deletes-dest/overwrite/bad-op/wrongtype)"
    )


if __name__ == "__main__":
    main()
