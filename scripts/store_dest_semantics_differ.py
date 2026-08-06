#!/usr/bin/env python3
"""Differential gate: STORE-family destination semantics (frankenredis-afkjq).

The *STORE commands share a subtle, bug-prone rule: when the operation result is
EMPTY, a pre-existing destination key is DELETED (and the command returns 0); when
non-empty, the dest is overwritten regardless of its previous type. This gate pins
both behaviors byte-exact vs redis 7.2.4 across SINTERSTORE / SUNIONSTORE /
SDIFFSTORE / ZINTERSTORE / ZUNIONSTORE / ZDIFFSTORE / SORT...STORE / ZRANGESTORE —
checking the reply, the resulting dest TYPE/EXISTS, and the stored contents
(including WEIGHTS / AGGREGATE for the zset ops).

Usage: store_dest_semantics_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import sys

from _respread import assert_seed, cmd, conn


# Each step: (label, argv). Steps run in order; "RESET" re-seeds both servers
# (including pre-existing dest keys of a string + list type to test overwrite).
STEPS = [
    "RESET",
    ("sinterstore_empty", ["SINTERSTORE", "dest", "s1", "s3"]),       # disjoint -> 0
    ("sinterstore_empty_gone", ["EXISTS", "dest"]),                   # dest deleted
    "RESET",
    ("sinterstore_nonempty", ["SINTERSTORE", "dest", "s1", "s2"]),    # overwrites string dest
    ("sinterstore_type", ["TYPE", "dest"]),
    ("sinterstore_members", ["SMEMBERS", "dest"]),
    "RESET",
    ("sunionstore", ["SUNIONSTORE", "dest", "s1", "s2"]),
    ("sunionstore_card", ["SCARD", "dest"]),
    "RESET",
    ("sdiffstore_empty", ["SDIFFSTORE", "dest", "s1", "s1"]),         # self-diff -> empty
    ("sdiffstore_empty_gone", ["EXISTS", "dest"]),
    "RESET",
    ("zinterstore", ["ZINTERSTORE", "dest", "2", "z1", "z2"]),
    ("zinterstore_type", ["TYPE", "dest"]),
    ("zinterstore_range", ["ZRANGE", "dest", "0", "-1", "WITHSCORES"]),
    "RESET",
    ("zinterstore_empty", ["ZINTERSTORE", "dest", "2", "z1", "s3"]),  # zset ∩ disjoint set
    ("zinterstore_empty_gone", ["EXISTS", "dest"]),
    "RESET",
    ("zunionstore_weights", ["ZUNIONSTORE", "dest", "2", "z1", "z2", "WEIGHTS", "2", "3"]),
    ("zunionstore_w_range", ["ZRANGE", "dest", "0", "-1", "WITHSCORES"]),
    "RESET",
    ("zunionstore_aggmin", ["ZUNIONSTORE", "dest", "2", "z1", "z2", "AGGREGATE", "MIN"]),
    ("zunionstore_agg_range", ["ZRANGE", "dest", "0", "-1", "WITHSCORES"]),
    "RESET",
    ("zdiffstore", ["ZDIFFSTORE", "dest", "2", "z1", "z2"]),
    ("zdiffstore_range", ["ZRANGE", "dest", "0", "-1", "WITHSCORES"]),
    "RESET",
    ("zdiffstore_empty", ["ZDIFFSTORE", "dest", "2", "z1", "z1"]),
    ("zdiffstore_empty_gone", ["EXISTS", "dest"]),
    "RESET",
    ("sort_store", ["SORT", "srt", "STORE", "dest"]),
    ("sort_store_type", ["TYPE", "dest"]),
    ("sort_store_range", ["LRANGE", "dest", "0", "-1"]),
    "RESET",
    ("sort_store_empty", ["SORT", "nolist", "STORE", "dest"]),        # empty src -> dest deleted
    ("sort_store_empty_gone", ["EXISTS", "dest"]),
    "RESET",
    ("zrangestore", ["ZRANGESTORE", "dest", "z1", "0", "-1"]),
    ("zrangestore_type", ["TYPE", "dest"]),
    "RESET",
    ("zrangestore_empty", ["ZRANGESTORE", "dest", "z1", "5", "10"]),  # OOB -> empty -> dest deleted
    ("zrangestore_empty_gone", ["EXISTS", "dest"]),
    # Cross-type overwrite of a NON-string dest. The docstring claimed a
    # list-typed dest was covered but reset() only ever created a string, so
    # "cross-type overwrite" was really only string->set. These make the
    # list-typed and zset-typed pre-existing dest real, in both the overwrite
    # and the empty-result-deletes directions.
    "RESET:list",
    ("overwrite_listdest_set", ["SINTERSTORE", "dest", "s1", "s2"]),
    ("overwrite_listdest_type", ["TYPE", "dest"]),
    ("overwrite_listdest_members", ["SMEMBERS", "dest"]),
    "RESET:list",
    ("emptyresult_listdest", ["SINTERSTORE", "dest", "s1", "s3"]),
    ("emptyresult_listdest_gone", ["EXISTS", "dest"]),
    ("emptyresult_listdest_type", ["TYPE", "dest"]),
    "RESET:zset",
    ("overwrite_zsetdest_sort", ["SORT", "srt", "STORE", "dest"]),
    ("overwrite_zsetdest_type", ["TYPE", "dest"]),
    ("overwrite_zsetdest_range", ["LRANGE", "dest", "0", "-1"]),
    "RESET:zset",
    ("emptyresult_zsetdest", ["ZDIFFSTORE", "dest", "2", "z1", "z1"]),
    ("emptyresult_zsetdest_gone", ["EXISTS", "dest"]),
    ("emptyresult_zsetdest_type", ["TYPE", "dest"]),
]


def reset(s, dest_kind="string"):
    """Re-seed both servers.

    EVERY seed is asserted. This gate is unusually exposed to a silent seed
    failure: if the sources were missing, every *STORE would produce an EMPTY
    result, dest would never be created, and every "empty deletes dest" check
    would see EXISTS 0 on BOTH engines. The whole gate would pass while
    exercising nothing at all. (frankenredis-r9ei8 mechanism 4)
    """
    cmd(s, "FLUSHALL")
    assert_seed(cmd(s, "SADD", "s1", "a", "b", "c"), 3, "SADD s1")
    assert_seed(cmd(s, "SADD", "s2", "c", "d", "e"), 3, "SADD s2")
    assert_seed(cmd(s, "SADD", "s3", "x", "y"), 2, "SADD s3")
    assert_seed(cmd(s, "ZADD", "z1", "1", "a", "2", "b"), 2, "ZADD z1")
    assert_seed(cmd(s, "ZADD", "z2", "3", "b", "4", "c"), 2, "ZADD z2")
    assert_seed(cmd(s, "RPUSH", "srt", "3", "1", "2"), 3, "RPUSH srt")
    # A PRE-EXISTING dest of a different type, so the overwrite cases actually
    # overwrite something. The docstring claimed a list-typed dest was covered;
    # it was not — only a string was ever created. dest_kind makes both real.
    if dest_kind == "string":
        if cmd(s, "SET", "dest", "preexisting-string") != b"+OK\r\n":
            print("SEED FAILED: SET dest")
            sys.exit(1)
    elif dest_kind == "list":
        assert_seed(cmd(s, "RPUSH", "dest", "old1", "old2"), 2, "RPUSH dest")
    elif dest_kind == "zset":
        assert_seed(cmd(s, "ZADD", "dest", "9", "old"), 1, "ZADD dest")
    else:
        raise ValueError(dest_kind)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    n = 0
    for step in STEPS:
        if isinstance(step, str) and step.startswith("RESET"):
            kind = step.split(":", 1)[1] if ":" in step else "string"
            reset(od, kind)
            reset(fr, kind)
            continue
        label, argv = step
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        n += 1
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} STORE-family divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — STORE-family destination semantics byte-exact vs redis 7.2.4 "
        f"({n} checks: empty-deletes-dest + cross-type overwrite + WEIGHTS/AGGREGATE)"
    )


if __name__ == "__main__":
    main()
