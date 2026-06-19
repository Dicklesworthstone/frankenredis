#!/usr/bin/env python3
"""Property gate: RANDOMKEY invariants (frankenredis-uhthd lazy RANDOMKEY side-index).

RANDOMKEY is random, so it canNOT be a redis-differential. This is a SINGLE-SERVER PROPERTY
gate asserting the contract RANDOMKEY must uphold no matter how the keyspace sampling index is
represented — it guards the uhthd "lazy RANDOMKEY side-index" lever (which dropped the per-Entry
random_slot u32 + the random_key_positions map in favour of a lazily-rebuilt
RandomKeySlotIndex) and any future keyspace-sampling change:

  1. on a populated db every RANDOMKEY result is a key that currently EXISTS;
  2. over many calls the results COVER the whole keyspace (sampling reaches every key);
  3. after DEL, RANDOMKEY NEVER returns a deleted key (the side-index is kept consistent);
  4. on an empty db RANDOMKEY returns nil;
  5. a single-key db always returns that key;
  6. all of the above still hold after DEBUG RELOAD (the index rebuilds lazily).

A regression here (a stale/dangling key from the sampling index, or a key it can never reach)
is a correctness bug a per-call oracle diff would miss.

Usage: randomkey_invariant_gate.py [<oracle_port>] <fr_port>   (LAST arg = fr subject; an
       oracle arg is accepted+ignored so it slots into parity_suite's PORT_BASED convention.)
       Exit 0 = invariants hold, 1 = violated.
"""
import re
import socket
import sys
import time

N = 300


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=8)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.005)
    return s.recv(1 << 20)


def bulk(r):
    if r[:1] != b"$":
        return None
    nl = r.index(b"\r\n")
    n = int(r[1:nl])
    return None if n < 0 else r[nl + 2:nl + 2 + n]


def randomkeys(s, calls):
    out = set()
    for _ in range(calls):
        k = bulk(cmd(s, "RANDOMKEY"))
        if k is not None:
            out.add(k)
    return out


def main():
    fp = int(sys.argv[-1]) if len(sys.argv) > 1 else 16400
    s = conn(fp)
    fails = []

    # 4. empty db -> nil
    cmd(s, "FLUSHALL")
    if bulk(cmd(s, "RANDOMKEY")) is not None:
        fails.append("empty db: RANDOMKEY not nil")

    # 5. single key -> always that key
    cmd(s, "SET", "solo", "v")
    for _ in range(50):
        if bulk(cmd(s, "RANDOMKEY")) != b"solo":
            fails.append("single key: RANDOMKEY != solo")
            break
    cmd(s, "FLUSHALL")

    # populate
    full = {f"k{i:04d}".encode() for i in range(N)}
    for i in range(N):
        cmd(s, "SET", f"k{i:04d}", "v")

    def check_round(label, present):
        seen = randomkeys(s, len(present) * 15)
        bad = seen - present
        if bad:
            fails.append(f"{label}: returned {len(bad)} non-present key(s) e.g. {list(bad)[:3]}")
        if seen != present:
            miss = present - seen
            fails.append(f"{label}: coverage incomplete, {len(miss)} key(s) never sampled "
                         f"e.g. {list(miss)[:3]}")

    # 1+2. valid + full coverage
    check_round("populated", full)

    # 3. after DEL half -> never a deleted key, remaining fully covered
    for i in range(0, N, 2):
        cmd(s, "DEL", f"k{i:04d}")
    remaining = {f"k{i:04d}".encode() for i in range(1, N, 2)}
    check_round("post_delete", remaining)

    # add fresh keys -> sampling reaches them too
    for i in range(N, N + 40):
        cmd(s, "SET", f"k{i:04d}", "v")
    remaining |= {f"k{i:04d}".encode() for i in range(N, N + 40)}
    check_round("post_add", remaining)

    # 6. after DEBUG RELOAD (lazy index rebuild) — conditional
    if cmd(s, "DEBUG", "RELOAD").startswith(b"+OK"):
        check_round("post_reload", remaining)

    if fails:
        print(f"FAIL — {len(fails)} RANDOMKEY-invariant violation(s):")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — RANDOMKEY invariant holds (valid + full coverage + no stale-after-DEL + "
          "empty->nil + single-key + post-RELOAD) [guards uhthd lazy RANDOMKEY side-index]")


if __name__ == "__main__":
    main()
