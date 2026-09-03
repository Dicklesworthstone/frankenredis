#!/usr/bin/env python3
"""Property gate: keyspace SCAN completeness/no-dup invariant (frankenredis-uhthd).

fr's SCAN cursor is a position into its own keyspace dict, distinct from redis's hash-bucket
reverse-binary cursor, so SCAN canNOT be checked against the redis oracle — a
redis-differential would false-positive on every run. Instead this is a SINGLE-SERVER
PROPERTY gate asserting the contract that fr's SCAN must uphold no matter how the underlying
keyspace dict is represented (it guards the uhthd arena-backed KeyDict rewrite + any future
KeyDict change): a full cursor chain returns EVERY key EXACTLY ONCE, and the set is stable
under COUNT variation, MATCH/TYPE filters, and mid-life mutation.

Order is NOT part of the contract. The first version of this gate also required sorted
output, which was a property of the BTreeSet side index the keyspace kept at the time; that
index was dropped in uhthd step 2 (`ordered_physical_keys_in_db` now sorts on demand for the
few callers that need order) because keeping a second owned copy of every key cost more RSS
than the whole rewrite saved. SCAN now walks the dict in arena order, which is what Redis
does too — Redis guarantees completeness and no duplicates for a stable keyspace and nothing
about order. (frankenredis-rc-keyspace-gates-l1h3w, 2026-09-03: every check reported
"NOT SORTED" and nothing else.)

A regression here (a key skipped or duplicated by SCAN) is a silent data-visibility bug that
a per-step oracle diff would miss.

Usage: scan_invariant_gate.py [<oracle_port>] <fr_port>   (the LAST arg is the fr subject;
       oracle arg accepted+ignored so it slots into parity_suite's PORT_BASED convention.)
       Exit 0 = invariant holds, 1 = violated.
"""
import re
import sys

from _respread import assert_ok, assert_seed, cmd, conn

N = 1000


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
    assert_ok(cmd(s, "FLUSHALL"), "FLUSHALL")
    expect = {f"k{i:05d}" for i in range(N)}
    for i in range(N):
        cmd(s, "SET", f"k{i:05d}", "v")
    # A short populate makes the completeness/no-dup checks below trivially
    # satisfiable — a single-cursor SCAN over 3 keys is "complete". Pin the size.
    # (frankenredis-tesrb)
    assert_seed(cmd(s, "DBSIZE"), N, f"populate {N} keys")

    def check(label, keys, want):
        if len(keys) != len(set(keys)):
            dups = [k for k in set(keys) if keys.count(k) > 1][:5]
            fails.append(f"{label}: DUPLICATES e.g. {dups}")
        if set(keys) != want:
            miss = list(want - set(keys))[:5]
            extra = list(set(keys) - want)[:5]
            fails.append(f"{label}: INCOMPLETE missing={miss} extra={extra}")

    for c in (1, 7, 100, 1000, 5000):
        check(f"count{c}", scan_all(s, c), expect)
    # MATCH filter: complete subset, no duplicates
    want_1 = {k for k in expect if k.startswith("k0001")}
    check("match", scan_all(s, 11, match="k0001*"), want_1)
    # TYPE filter: all are strings -> full set
    check("type_string", scan_all(s, 13, typ="string"), expect)
    # mutation: delete a swath, re-scan -> remaining complete/no-dup
    for i in range(0, N, 3):
        cmd(s, "DEL", f"k{i:05d}")
    remaining = {k for k in expect if int(k[1:]) % 3 != 0}
    check("after_delete", scan_all(s, 9, match="k*"), remaining)
    # add a different type, re-scan full
    assert_seed(cmd(s, "RPUSH", "alist", "x"), 1, "RPUSH alist")
    assert_seed(cmd(s, "HSET", "ahash", "f", "v"), 1, "HSET ahash")
    full = remaining | {"alist", "ahash"}
    check("after_mixed_add", scan_all(s, 17), full)
    # (frankenredis-uhthd presized KeyDict build) DEBUG RELOAD rebuilds the keyspace dict
    # via the PRESIZED bulk build; the SCAN invariant + DBSIZE must survive it. Conditional:
    # skip cleanly if DEBUG is disabled on this server (so the gate is portable).
    reload_reply = cmd(s, "DEBUG", "RELOAD")
    if reload_reply.startswith(b"+OK"):
        db = cmd(s, "DBSIZE")
        try:
            n = int(db.split(b"\r\n")[0][1:])
        except ValueError:
            n = -1
        if n != len(full):
            fails.append(f"after_reload: DBSIZE {n} != {len(full)}")
        check("after_reload", scan_all(s, 13), full)
    elif b"DEBUG command not allowed" not in reload_reply and b"unknown" not in reload_reply.lower():
        fails.append(f"after_reload: unexpected DEBUG RELOAD reply {reload_reply[:40]!r}")

    if fails:
        print(f"FAIL — {len(fails)} SCAN-invariant violation(s):")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — keyspace SCAN invariant holds (complete + no-dup across "
          "COUNT 1..5000, MATCH/TYPE filters, post-delete, mixed types, post-DEBUG-RELOAD presized rebuild) [guards uhthd KeyDict]")


if __name__ == "__main__":
    main()
