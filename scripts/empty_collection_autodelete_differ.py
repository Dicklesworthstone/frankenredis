#!/usr/bin/env python3
"""Differential gate: empty-collection auto-delete invariant (frankenredis-mh1rq).

redis deletes a key the moment its aggregate collection becomes empty — so EXISTS/
TYPE flip to 0/none right after the last element is removed, through EVERY removal
path. A type/encoding-specific code path that left an empty container behind would
break this (and corrupt RDB / keyspace / WAIT semantics). This gate removes the
last element via each path and asserts the command reply + post-state EXISTS/TYPE
match redis 7.2.4. It also pins the deliberate exception: XDEL of the last stream
entry leaves the (now-empty) stream in place.

Usage: empty_collection_autodelete_differ.py <oracle_port> <fr_port>
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


# (label, seed-commands, removal-command, key, expect_key_after)
# seed-commands are run on a freshly-DEL'd key; then the removal command; then
# EXISTS + TYPE on the key are compared. expect_key_after is documentation only.
SCENARIOS = [
    ("lpop_last", [["RPUSH", "l", "a"]], ["LPOP", "l"], "l"),
    ("rpop_last", [["RPUSH", "l", "a"]], ["RPOP", "l"], "l"),
    ("lrem_all", [["RPUSH", "l", "a", "a"]], ["LREM", "l", "0", "a"], "l"),
    ("ltrim_to_empty", [["RPUSH", "l", "a", "b", "c"]], ["LTRIM", "l", "5", "1"], "l"),
    ("lpop_count_all", [["RPUSH", "l", "a", "b"]], ["LPOP", "l", "5"], "l"),
    ("srem_last", [["SADD", "st", "a"]], ["SREM", "st", "a"], "st"),
    ("spop_last", [["SADD", "st", "a"]], ["SPOP", "st"], "st"),
    ("spop_count_all", [["SADD", "st", "a", "b"]], ["SPOP", "st", "5"], "st"),
    ("hdel_last", [["HSET", "h", "f", "v"]], ["HDEL", "h", "f"], "h"),
    ("zrem_last", [["ZADD", "z", "1", "a"]], ["ZREM", "z", "a"], "z"),
    ("zpopmin_last", [["ZADD", "z", "1", "a"]], ["ZPOPMIN", "z"], "z"),
    ("zpopmax_last", [["ZADD", "z", "1", "a"]], ["ZPOPMAX", "z"], "z"),
    ("zremrangebyscore_all", [["ZADD", "z", "1", "a", "2", "b"]],
     ["ZREMRANGEBYSCORE", "z", "-inf", "+inf"], "z"),
    ("zremrangebyrank_all", [["ZADD", "z", "1", "a", "2", "b"]],
     ["ZREMRANGEBYRANK", "z", "0", "-1"], "z"),
    ("zremrangebylex_all", [["ZADD", "z", "0", "a", "0", "b"]],
     ["ZREMRANGEBYLEX", "z", "-", "+"], "z"),
    # deliberate exception: XDEL of the last entry does NOT delete the stream
    ("xdel_last_keeps_stream", [["XADD", "x", "1-1", "f", "v"]], ["XDEL", "x", "1-1"], "x"),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    for label, seed, removal, key in SCENARIOS:
        for s in (od, fr):
            cmd(s, "DEL", key, "sb")  # sb is the SMOVE dest target if used
            for sc in seed:
                cmd(s, *sc)
        ro, rf = cmd(od, *removal), cmd(fr, *removal)
        eo, ef = cmd(od, "EXISTS", key), cmd(fr, "EXISTS", key)
        to, tf = cmd(od, "TYPE", key), cmd(fr, "TYPE", key)
        if ro != rf:
            fails.append(f"{label} reply: redis={ro!r} fr={rf!r}")
        if eo != ef:
            fails.append(f"{label} EXISTS: redis={eo!r} fr={ef!r}")
        if to != tf:
            fails.append(f"{label} TYPE: redis={to!r} fr={tf!r}")
    # SMOVE: moving the last member deletes the source key
    for s in (od, fr):
        cmd(s, "DEL", "sa", "sb")
        cmd(s, "SADD", "sa", "x")
    ro, rf = cmd(od, "SMOVE", "sa", "sb", "x"), cmd(fr, "SMOVE", "sa", "sb", "x")
    eo, ef = cmd(od, "EXISTS", "sa"), cmd(fr, "EXISTS", "sa")
    if ro != rf:
        fails.append(f"smove_last reply: redis={ro!r} fr={rf!r}")
    if eo != ef:
        fails.append(f"smove_last src EXISTS: redis={eo!r} fr={ef!r}")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} auto-delete divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — empty-collection auto-delete invariant byte-exact vs redis 7.2.4 "
        f"({len(SCENARIOS)} paths + SMOVE: list/set/hash/zset auto-delete, XDEL keeps stream)"
    )


if __name__ == "__main__":
    main()
