#!/usr/bin/env python3
"""Property gate: keyspace SCAN completeness/order/no-dup invariant (frankenredis-uhthd).

fr's SCAN is DELIBERATELY deterministic (sorted-order + index cursor), distinct from redis's
hash-bucket reverse-binary cursor, so SCAN canNOT be checked against the redis oracle — a
redis-differential would false-positive on every run. Instead this is a SINGLE-SERVER
PROPERTY gate asserting the contract that fr's SCAN must uphold no matter how the underlying
keyspace dict is represented (it guards the uhthd arena-backed KeyDict rewrite + any future
KeyDict change): a full cursor chain returns EVERY key EXACTLY ONCE, in sorted order, and the
set is stable under COUNT variation, MATCH/TYPE filters, and mid-life mutation.

A regression here (a key skipped or duplicated by SCAN) is a silent data-visibility bug that
a per-step oracle diff would miss.

Usage: scan_invariant_gate.py [<oracle_port>] <fr_port>   (the LAST arg is the fr subject;
       oracle arg accepted+ignored so it slots into parity_suite's PORT_BASED convention.)
       Exit 0 = invariant holds, 1 = violated.
"""
import re
import socket
import sys
import time

N = 1000


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=8)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.01)
    return s.recv(1 << 20)


def scan_all(s, count, match=None, typ=None):
    cur, keys, iters = "0", [], 0
    while True:
        args = ["SCAN", cur, "COUNT", str(count)]
        if match:
            args += ["MATCH", match]
        if typ:
            args += ["TYPE", typ]
        r = cmd(s, *args)
        iters += 1
        parts = re.findall(rb"\$\d+\r\n([^\r]*)\r\n", r)
        if not parts:
            break
        cur = parts[0].decode()
        keys += [p.decode() for p in parts[1:]]
        if cur == "0" or iters > 100000:
            break
    return keys


def main():
    fp = int(sys.argv[-1]) if len(sys.argv) > 1 else 16400
    s = conn(fp)
    fails = []
    cmd(s, "FLUSHALL")
    expect = {f"k{i:05d}" for i in range(N)}
    for i in range(N):
        cmd(s, "SET", f"k{i:05d}", "v")

    def check(label, keys, want):
        if len(keys) != len(set(keys)):
            dups = [k for k in set(keys) if keys.count(k) > 1][:5]
            fails.append(f"{label}: DUPLICATES e.g. {dups}")
        if set(keys) != want:
            miss = list(want - set(keys))[:5]
            extra = list(set(keys) - want)[:5]
            fails.append(f"{label}: INCOMPLETE missing={miss} extra={extra}")
        if keys != sorted(keys):
            fails.append(f"{label}: NOT SORTED")

    for c in (1, 7, 100, 1000, 5000):
        check(f"count{c}", scan_all(s, c), expect)
    # MATCH filter: complete subset, sorted
    want_1 = {k for k in expect if k.startswith("k0001")}
    check("match", scan_all(s, 11, match="k0001*"), want_1)
    # TYPE filter: all are strings -> full set
    check("type_string", scan_all(s, 13, typ="string"), expect)
    # mutation: delete a swath, re-scan -> remaining complete/sorted/no-dup
    for i in range(0, N, 3):
        cmd(s, "DEL", f"k{i:05d}")
    remaining = {k for k in expect if int(k[1:]) % 3 != 0}
    check("after_delete", scan_all(s, 9, match="k*"), remaining)
    # add a different type, re-scan full
    cmd(s, "RPUSH", "alist", "x")
    cmd(s, "HSET", "ahash", "f", "v")
    check("after_mixed_add", scan_all(s, 17), remaining | {"alist", "ahash"})

    if fails:
        print(f"FAIL — {len(fails)} SCAN-invariant violation(s):")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — keyspace SCAN invariant holds (complete + no-dup + sorted across "
          "COUNT 1..5000, MATCH/TYPE filters, post-delete, mixed types) [guards uhthd KeyDict]")


if __name__ == "__main__":
    main()
