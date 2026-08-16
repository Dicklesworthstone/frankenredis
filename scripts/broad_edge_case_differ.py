#!/usr/bin/env python3
"""Broad edge-case differential gate: fr vs live vendored Redis 7.2.4.

Forty-nine probes over the places a reimplementation typically drifts: boundary
arguments, empty and absent containers, option interactions that are individually
legal but jointly rejected, and commands whose reply SHAPE changes with an option.
Families covered: LPOS RANK/COUNT/MAXLEN, SETRANGE and GETRANGE boundaries,
SINTERCARD LIMIT, the ZADD NX/XX/GT/LT/INCR matrix, ZRANGEBYSCORE and ZRANGEBYLEX
and ZRANGE BYLEX/BYSCORE/REV/LIMIT, SPOP and SRANDMEMBER counts, the EXPIRE
NX/XX/GT/LT matrix, OBJECT ENCODING across encodings, SET and GETEX option
interactions, LMPOP and ZMPOP shapes, and SETEX/PSETEX TTL edges.

Usage: broad_edge_case_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = byte-exact on every probe, 1 = divergence.

Written after a FUNCTION-family sweep found frankenredis-niu8g (a silent
function-name shadowing bug) in minutes: a differential sweep is the cheapest way
this campaign has to convert idle capacity into correctness findings, and a CLEAN
sweep is itself evidence — it bounds where the divergences are not.
"""
import sys

from _respread import cmd, conn

SETUP = [
    ("FLUSHALL",),
    ("RPUSH", "l", "a", "b", "c", "b", "a"),
    ("SET", "s", "hello"),
    ("SADD", "st", "a", "b", "c"),
    ("ZADD", "z", "1", "a", "2", "b", "3", "c"),
    ("HSET", "h", "f1", "v1", "f2", "v2"),
    ("SETRANGE", "pad", "5", "xy"),
]

PROBES = [
    # LPOS option interactions
    ("lpos rank neg count0", ("LPOS", "l", "b", "RANK", "-1", "COUNT", "0")),
    ("lpos count0 maxlen1", ("LPOS", "l", "a", "COUNT", "0", "MAXLEN", "1")),
    ("lpos rank0", ("LPOS", "l", "a", "RANK", "0")),
    ("lpos maxlen neg", ("LPOS", "l", "a", "MAXLEN", "-1")),
    # SETRANGE / GETRANGE boundaries
    ("getrange past end", ("GETRANGE", "s", "10", "20")),
    ("getrange rev", ("GETRANGE", "s", "-1", "-5")),
    ("getrange whole neg", ("GETRANGE", "s", "-100", "-1")),
    ("setrange empty at 0", ("SETRANGE", "s", "0", "")),
    ("setrange absent empty", ("SETRANGE", "nosuch", "0", "")),
    # SINTERCARD limits
    ("sintercard limit0", ("SINTERCARD", "1", "st", "LIMIT", "0")),
    ("sintercard limit neg", ("SINTERCARD", "1", "st", "LIMIT", "-1")),
    ("sintercard numkeys0", ("SINTERCARD", "0")),
    # ZADD option interactions
    ("zadd gt lt", ("ZADD", "z", "GT", "LT", "5", "a")),
    ("zadd nx xx", ("ZADD", "z", "NX", "XX", "5", "a")),
    ("zadd nx gt", ("ZADD", "z", "NX", "GT", "5", "a")),
    ("zadd incr nan", ("ZADD", "z", "INCR", "inf", "a")),
    ("zadd incr multi", ("ZADD", "z", "INCR", "1", "a", "2", "b")),
    # ZRANGEBYSCORE / LEX edges
    ("zrangebyscore inf", ("ZRANGEBYSCORE", "z", "-inf", "+inf")),
    ("zrangebyscore excl", ("ZRANGEBYSCORE", "z", "(1", "(3")),
    ("zrangebylex bad", ("ZRANGEBYLEX", "z", "a", "c")),
    ("zrangebylex minus plus", ("ZRANGEBYLEX", "z", "-", "+")),
    ("zrange rev limit", ("ZRANGE", "z", "(1", "(3", "BYSCORE", "REV", "LIMIT", "0", "1")),
    ("zrange bylex rev", ("ZRANGE", "z", "+", "-", "BYLEX", "REV")),
    # SRANDMEMBER / negative counts
    ("srandmember 0", ("SRANDMEMBER", "st", "0")),
    ("spop 0", ("SPOP", "st", "0")),
    ("srandmember neg big", ("SRANDMEMBER", "nosuchset", "-3")),
    # EXPIRE option matrix
    ("expire nx on persistent", ("EXPIRE", "s", "100", "NX")),
    ("expire nx again", ("EXPIRE", "s", "200", "NX")),
    ("expire gt", ("EXPIRE", "s", "50", "GT")),
    ("expire lt", ("EXPIRE", "s", "50", "LT")),
    ("expire nx xx", ("EXPIRE", "s", "10", "NX", "XX")),
    ("expire negative", ("EXPIRE", "nosuchkey", "-1")),
    # OBJECT / type
    ("object enc list", ("OBJECT", "ENCODING", "l")),
    ("object enc zset", ("OBJECT", "ENCODING", "z")),
    ("object enc hash", ("OBJECT", "ENCODING", "h")),
    ("object enc str", ("OBJECT", "ENCODING", "s")),
    ("object enc padded", ("OBJECT", "ENCODING", "pad")),
    # GETEX / SET option interactions
    ("getex exat 0", ("GETEX", "s", "EXAT", "0")),
    ("set keepttl idle", ("SET", "s2", "v", "KEEPTTL")),
    ("set xx get absent", ("SET", "nosuch2", "v", "XX", "GET")),
    ("set nx get existing", ("SET", "s2", "w", "NX", "GET")),
    ("set exat past", ("SET", "s3", "v", "EXAT", "1")),
    ("exists after past exat", ("EXISTS", "s3")),
    # LMPOP / ZMPOP shapes
    ("lmpop 1", ("LMPOP", "1", "l", "LEFT", "COUNT", "2")),
    ("zmpop min count", ("ZMPOP", "1", "z", "MIN", "COUNT", "2")),
    ("lmpop count0", ("LMPOP", "1", "l", "LEFT", "COUNT", "0")),
    # SETEX / bad TTLs
    ("setex zero", ("SETEX", "k", "0", "v")),
    ("setex negative", ("SETEX", "k", "-1", "v")),
    ("psetex zero", ("PSETEX", "k", "0", "v")),
]


def run(sock):
    for setup in SETUP:
        cmd(sock, *setup)
    return [(name, cmd(sock, *args)) for name, args in PROBES]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    rd = run(conn(op))
    fr = run(conn(fp))
    diffs = 0
    for (name, f), (_, r) in zip(fr, rd):
        if f != r:
            diffs += 1
            print("DIFF %-26s" % name)
            print("   fr    %r" % f[:120])
            print("   redis %r" % r[:120])
    if diffs:
        print("\nFAIL — %d of %d probes diverge from redis 7.2.4" % (diffs, len(PROBES)))
        return 1
    print("PASS — all %d edge-case probes byte-exact vs redis 7.2.4" % len(PROBES))
    return 0


if __name__ == "__main__":
    sys.exit(main())
