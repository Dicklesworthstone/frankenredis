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
]


def reset(s):
    cmd(s, "FLUSHALL")
    cmd(s, "SADD", "s1", "a", "b", "c")
    cmd(s, "SADD", "s2", "c", "d", "e")
    cmd(s, "SADD", "s3", "x", "y")
    cmd(s, "ZADD", "z1", "1", "a", "2", "b")
    cmd(s, "ZADD", "z2", "3", "b", "4", "c")
    cmd(s, "RPUSH", "srt", "3", "1", "2")
    cmd(s, "SET", "dest", "preexisting-string")   # dest exists as a string


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
