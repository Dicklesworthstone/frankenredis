#!/usr/bin/env python3
"""Differential gate: list mutation edge cases (frankenredis-i7ngc).

LREM / LINSERT / LSET / LTRIM are fully deterministic but carry subtle semantics:
  * LREM count: >0 removes from head, <0 from tail, 0 removes all; count may exceed
    occurrences; missing element/key -> 0
  * LINSERT BEFORE|AFTER pivot: pivot-not-found -> -1, missing key -> 0, bad
    direction -> syntax error, direction is case-insensitive
  * LSET: negative index, out-of-range -> error, missing key -> error
  * LTRIM: negative indices, a range that empties the list DELETES the key
Each case checks BOTH the command reply AND the resulting list (LRANGE 0 -1)
byte-exact vs vendored redis 7.2.4.

Usage: list_mutation_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

BASE = ["a", "b", "a", "c", "a", "b", "a"]  # a x4, b x2, c x1


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


# (label, list-key, reset_items_or_None, command-argv)
CASES = [
    ("lrem_pos2_a", "l", BASE, ["LREM", "l", "2", "a"]),
    ("lrem_pos_overflow", "l", BASE, ["LREM", "l", "100", "a"]),
    ("lrem_neg2_a", "l", BASE, ["LREM", "l", "-2", "a"]),
    ("lrem_neg_overflow", "l", BASE, ["LREM", "l", "-100", "a"]),
    ("lrem_zero_all", "l", BASE, ["LREM", "l", "0", "a"]),
    ("lrem_neg1_b", "l", BASE, ["LREM", "l", "-1", "b"]),
    ("lrem_missing_elem", "l", BASE, ["LREM", "l", "2", "zzz"]),
    ("lrem_missing_key", "nokey", [], ["LREM", "nokey", "2", "a"]),
    ("linsert_before_b", "l", BASE, ["LINSERT", "l", "BEFORE", "b", "X"]),
    ("linsert_after_b", "l", BASE, ["LINSERT", "l", "AFTER", "b", "X"]),
    ("linsert_before_head", "l", BASE, ["LINSERT", "l", "BEFORE", "a", "X"]),
    ("linsert_after_tail", "l", BASE, ["LINSERT", "l", "AFTER", "c", "X"]),
    ("linsert_pivot_missing", "l", BASE, ["LINSERT", "l", "BEFORE", "zzz", "X"]),
    ("linsert_lowercase_dir", "l", BASE, ["LINSERT", "l", "before", "b", "X"]),
    ("linsert_bad_dir", "l", BASE, ["LINSERT", "l", "SIDEWAYS", "b", "X"]),
    ("linsert_missing_key", "nokey", [], ["LINSERT", "nokey", "BEFORE", "a", "X"]),
    ("lset_0", "l", BASE, ["LSET", "l", "0", "Z"]),
    ("lset_neg1", "l", BASE, ["LSET", "l", "-1", "Z"]),
    ("lset_oob", "l", BASE, ["LSET", "l", "100", "Z"]),
    ("lset_missing_key", "nokey", [], ["LSET", "nokey", "0", "Z"]),
    ("ltrim_1_3", "l", BASE, ["LTRIM", "l", "1", "3"]),
    ("ltrim_neg", "l", BASE, ["LTRIM", "l", "-3", "-1"]),
    ("ltrim_empties_key", "l", BASE, ["LTRIM", "l", "5", "1"]),
    ("ltrim_oob", "l", BASE, ["LTRIM", "l", "0", "100"]),
    ("lrem_wrongtype", "str", None, ["LREM", "str", "1", "a"]),
    ("linsert_wrongtype", "str", None, ["LINSERT", "str", "BEFORE", "a", "X"]),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "str", "x")
    fails = []
    for label, key, reset_items, argv in CASES:
        if reset_items is not None:
            for s in (od, fr):
                cmd(s, "DEL", key)
                if reset_items:
                    cmd(s, "RPUSH", key, *reset_items)
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        lo, lf = cmd(od, "LRANGE", key, "0", "-1"), cmd(fr, "LRANGE", key, "0", "-1")
        if ro != rf:
            fails.append(f"{label} reply: redis={ro!r} fr={rf!r}")
        if lo != lf:
            fails.append(f"{label} list: redis={lo!r} fr={lf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} list-mutation divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — list mutations byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases x reply+resulting-list: LREM/LINSERT/LSET/LTRIM)"
    )


if __name__ == "__main__":
    main()
