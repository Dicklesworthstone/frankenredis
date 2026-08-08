#!/usr/bin/env python3
"""Differential gate: TTL-on-mutation invariant (frankenredis-aggm6).

A key's TTL SURVIVES an in-place modification (APPEND/SETRANGE/SETBIT/INCR*/LPUSH/
RPUSH/LSET/SADD/SREM/HSET/HINCRBY/ZADD/ZINCRBY/XADD/SPOP) but is CLEARED by a full
overwrite (SET without KEEPTTL, GETSET); SET ... KEEPTTL preserves it and SET ... EX
replaces it. This invariant is distinct from ttl_semantics (which probes the TTL
*commands*). This gate sets an absolute TTL (EXPIREAT to a fixed far-future second),
runs the mutation, and asserts the resulting EXPIRETIME/TTL byte-exact vs redis
7.2.4 — plus PERSIST. The mutation reply is compared only when deterministic (SPOP
returns a random member, so only its TTL effect is checked).

Usage: ttl_on_mutation_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import sys

from _respread import assert_ok, assert_seed, cmd, conn

EXAT = "4102444800"  # 2100-01-01 absolute seconds


# (label, seed-commands, mutation, ttl_check, deterministic_reply)
CASES = [
    ("append", [["SET", "k", "hello"]], ["APPEND", "k", "X"], "EXPIRETIME", True),
    ("setrange", [["SET", "k", "hello"]], ["SETRANGE", "k", "1", "X"], "EXPIRETIME", True),
    ("setbit", [["SET", "k", "hello"]], ["SETBIT", "k", "0", "1"], "EXPIRETIME", True),
    ("incr", [["SET", "k", "10"]], ["INCR", "k"], "EXPIRETIME", True),
    ("incrby", [["SET", "k", "10"]], ["INCRBY", "k", "5"], "EXPIRETIME", True),
    ("incrbyfloat", [["SET", "k", "10"]], ["INCRBYFLOAT", "k", "1.5"], "EXPIRETIME", True),
    ("lpush", [["RPUSH", "k", "a"]], ["LPUSH", "k", "b"], "EXPIRETIME", True),
    ("rpush", [["RPUSH", "k", "a"]], ["RPUSH", "k", "b"], "EXPIRETIME", True),
    ("lset", [["RPUSH", "k", "a", "b"]], ["LSET", "k", "0", "z"], "EXPIRETIME", True),
    ("lpop_partial", [["RPUSH", "k", "a", "b"]], ["LPOP", "k"], "EXPIRETIME", True),
    ("sadd", [["SADD", "k", "a"]], ["SADD", "k", "b"], "EXPIRETIME", True),
    ("srem_partial", [["SADD", "k", "a", "b"]], ["SREM", "k", "a"], "EXPIRETIME", True),
    ("spop_partial", [["SADD", "k", "a", "b", "c"]], ["SPOP", "k"], "EXPIRETIME", False),
    ("hset", [["HSET", "k", "f", "v"]], ["HSET", "k", "g", "w"], "EXPIRETIME", True),
    ("hincrby", [["HSET", "k", "f", "1"]], ["HINCRBY", "k", "f", "2"], "EXPIRETIME", True),
    ("hdel_partial", [["HSET", "k", "f", "1", "g", "2"]], ["HDEL", "k", "f"], "EXPIRETIME", True),
    ("zadd", [["ZADD", "k", "1", "a"]], ["ZADD", "k", "2", "b"], "EXPIRETIME", True),
    ("zincrby", [["ZADD", "k", "1", "a"]], ["ZINCRBY", "k", "2", "a"], "EXPIRETIME", True),
    ("xadd", [["XADD", "k", "1-1", "f", "v"]], ["XADD", "k", "2-2", "f", "v"], "EXPIRETIME", True),
    # full overwrite clears TTL
    ("set_overwrite", [["SET", "k", "v"]], ["SET", "k", "v2"], "TTL", True),
    ("getset", [["SET", "k", "v"]], ["GETSET", "k", "v2"], "TTL", True),
    # KEEPTTL preserves, SET EX replaces
    ("set_keepttl", [["SET", "k", "v"]], ["SET", "k", "v2", "KEEPTTL"], "EXPIRETIME", True),
    ("set_ex_resets", [["SET", "k", "v"]], ["SET", "k", "v2", "EX", "500"], "EXPIRETIME", True),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    for label, seed, mutation, ttl_check, det_reply in CASES:
        for s in (od, fr):
            cmd(s, "DEL", "k")
            for c in seed:
                cmd(s, *c)
            # EXPIREAT answers 1 only if the seed actually created `k`, so this one
            # assertion covers every per-case seed shape at once. Without it a
            # failed seed leaves EXPIRETIME/TTL at -2 on BOTH engines and the
            # invariant check passes having observed no TTL. (frankenredis-tesrb)
            assert_seed(cmd(s, "EXPIREAT", "k", EXAT), 1, f"{label} seed + EXPIREAT")
        ro, rf = cmd(od, *mutation), cmd(fr, *mutation)
        if det_reply and ro != rf:
            fails.append(f"{label}_reply: redis={ro!r} fr={rf!r}")
        # the invariant: TTL state after the mutation
        chk(f"{label}_ttl", ttl_check, "k")
    # PERSIST
    for s in (od, fr):
        cmd(s, "DEL", "k")
        assert_ok(cmd(s, "SET", "k", "v"), "SET k v")
        cmd(s, "EXPIREAT", "k", EXAT)
    chk("persist_had", "PERSIST", "k")
    chk("persist_ttl", "TTL", "k")
    chk("persist_no_ttl", "PERSIST", "k")
    chk("persist_missing", "PERSIST", "nope")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} TTL-on-mutation divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — TTL-on-mutation invariant byte-exact vs redis 7.2.4 "
        f"({len(CASES)} mutations: in-place preserves / overwrite clears / KEEPTTL / SET-EX / PERSIST)"
    )


if __name__ == "__main__":
    main()
