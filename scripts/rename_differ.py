#!/usr/bin/env python3
"""Differential gate: RENAME / RENAMENX semantics (frankenredis-8q1dz).

RENAME moves src to dst (overwriting dst), carrying src's TTL — so dst's prior TTL
is replaced, and renaming a no-TTL key onto a key that had a TTL clears it. RENAMENX
only succeeds if dst does not exist (returns 0 otherwise, including src==dst).
A missing src is an error ("no such key"); src==dst on RENAME is a no-op that keeps
the TTL; the dst may be a different type (overwritten). RENAME appears only in
GETKEYS / notification / random fuzzers — never a dedicated semantics gate. This
pins it byte-exact vs redis 7.2.4 using the absolute EXPIRETIME for deterministic
TTL checks.

Usage: rename_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import sys

from _respread import assert_ok, assert_seed, cmd, conn

EXAT = "4102444800"  # 2100-01-01 absolute seconds


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []

    def setup():
        for s in (od, fr):
            assert_ok(cmd(s, "FLUSHALL"), "FLUSHALL")
            # a: value + absolute TTL
            assert_ok(cmd(s, "SET", "a", "va", "EXAT", EXAT), "SET a EXAT")
            assert_ok(cmd(s, "SET", "b", "vb"), "SET b")                # b: no TTL
            assert_seed(cmd(s, "RPUSH", "lst", "x", "y"), 2, "RPUSH lst")
            assert_ok(cmd(s, "SET", "plain", "old"), "SET plain")

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    # basic rename + TTL preserved + src gone
    setup()
    chk("rename_ok", "RENAME", "a", "a2")
    chk("rename_val", "GET", "a2")
    chk("rename_ttl_preserved", "EXPIRETIME", "a2")
    chk("rename_src_gone", "EXISTS", "a")
    # overwrite dst: dst's TTL replaced by src's
    setup()
    cmd(od, "EXPIRE", "b", "999")
    cmd(fr, "EXPIRE", "b", "999")
    chk("rename_overwrite", "RENAME", "a", "b")
    chk("rename_overwrite_val", "GET", "b")
    chk("rename_overwrite_ttl", "EXPIRETIME", "b")
    # no-TTL src onto (any) dst -> result has no TTL
    setup()
    chk("rename_b_to_a", "RENAME", "b", "a")
    chk("rename_b_to_a_ttl", "TTL", "a")            # -1
    # src == dst no-op, TTL preserved
    setup()
    chk("rename_self", "RENAME", "a", "a")
    chk("rename_self_ttl", "EXPIRETIME", "a")
    # missing src -> error
    chk("rename_missing", "RENAME", "nope", "x")
    chk("rename_missing_self", "RENAME", "nope", "nope")
    # cross-type overwrite
    setup()
    chk("rename_list_over_str", "RENAME", "lst", "plain")
    chk("rename_list_over_str_type", "TYPE", "plain")
    # RENAMENX
    setup(); chk("renamenx_new", "RENAMENX", "a", "newkey"); chk("renamenx_new_val", "GET", "newkey")
    setup(); chk("renamenx_dst_exists", "RENAMENX", "a", "b"); chk("renamenx_dst_b", "GET", "b")
    chk("renamenx_missing", "RENAMENX", "nope", "x")
    setup(); chk("renamenx_self", "RENAMENX", "a", "a")
    # arity
    chk("rename_arity", "RENAME", "a")
    chk("renamenx_arity", "RENAMENX", "a")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} RENAME/RENAMENX divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — RENAME/RENAMENX semantics byte-exact vs redis 7.2.4 "
        "(TTL preserve/replace/clear, self-noop, missing-err, cross-type, RENAMENX, arity)"
    )


if __name__ == "__main__":
    main()
