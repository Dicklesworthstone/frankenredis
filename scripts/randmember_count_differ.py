#!/usr/bin/env python3
"""Differential gate: SRANDMEMBER/HRANDFIELD/ZRANDMEMBER count semantics (frankenredis-ljsd1).

These commands return RANDOM elements, so exact bytes can't be compared — but their
COUNT semantics are deterministic and a classic bug source:
  * positive count  -> up to `count` DISTINCT elements (capped at the collection size)
  * negative count  -> exactly `|count|` elements WITH replacement (may exceed size)
  * count >= size    -> the WHOLE collection (set-equal, order random)
  * count == 0       -> empty array
  * no-count form    -> single element (nil/empty on missing key)
  * WITHVALUES (HRANDFIELD) / WITHSCORES (ZRANDMEMBER) -> flattened pairs
This gate asserts fr matches redis on the deterministic invariants: the full-set
overflow case is compared byte-exact after sorting; everything else is checked by
cardinality / structure / error-reply equality.

Usage: randmember_count_differ.py <oracle_port> <fr_port>
       Exit 0 = invariants match, 1 = divergence.
"""
import socket
import sys
import time

SIZE = 5


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


def parr(b):
    if b.startswith((b"$-1", b"*-1")):
        return None
    if not b.startswith(b"*"):
        return ("RAW", b)
    n = int(b[1 : b.index(b"\r\n")])
    i = b.index(b"\r\n") + 2
    out = []
    for _ in range(n):
        if b[i : i + 1] == b"$":
            ln = int(b[i + 1 : b.index(b"\r\n", i)])
            i = b.index(b"\r\n", i) + 2
            out.append(b[i : i + ln])
            i += ln + 2
        else:
            j = b.index(b"\r\n", i)
            out.append(b[i:j])
            i = j + 2
    return out


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "SADD", "s", *[f"m{i}" for i in range(SIZE)])
        cmd(s, "HSET", "h", *sum([[f"f{i}", f"v{i}"] for i in range(SIZE)], []))
        cmd(s, "ZADD", "z", *sum([[str(i), f"zm{i}"] for i in range(SIZE)], []))
        cmd(s, "SET", "str", "x")
    fails = []

    def eq(label, a, b):
        if a != b:
            fails.append(f"{label}: redis={a!r} fr={b!r}")

    def card(srv, *c):
        r = parr(cmd(srv, *c))
        return len(r) if isinstance(r, list) else r

    def sorted_set(srv, *c):
        r = parr(cmd(srv, *c))
        return tuple(sorted(r)) if isinstance(r, list) else r

    CMDS = [("SRANDMEMBER", "s"), ("HRANDFIELD", "h"), ("ZRANDMEMBER", "z")]

    # overflow / exact-size positive count -> whole collection (sorted byte-exact)
    for name, key in CMDS:
        for n in ("5", "100"):
            eq(f"{name}_full{n}", sorted_set(od, name, key, n), sorted_set(fr, name, key, n))
    # cardinality: positive (capped at size, distinct) + negative (exactly |n|)
    for n in (1, 3, 5, 6, 100):
        for name, key in CMDS:
            eq(f"{name}_pos{n}_card", card(od, name, key, str(n)), card(fr, name, key, str(n)))
            eq(f"{name}_neg{n}_card", card(od, name, key, str(-n)), card(fr, name, key, str(-n)))
    # count 0, missing key (no-count nil + count empty + negcount), errors
    for name, key in CMDS:
        eq(f"{name}_zero", cmd(od, name, key, "0"), cmd(fr, name, key, "0"))
        eq(f"{name}_nocount_missing", cmd(od, name, "nope"), cmd(fr, name, "nope"))
        eq(f"{name}_count_missing", cmd(od, name, "nope", "3"), cmd(fr, name, "nope", "3"))
        eq(f"{name}_negcount_missing", cmd(od, name, "nope", "-3"), cmd(fr, name, "nope", "-3"))
        eq(f"{name}_notint", cmd(od, name, key, "abc"), cmd(fr, name, key, "abc"))
        eq(f"{name}_wrongtype", cmd(od, name, "str", "3"), cmd(fr, name, "str", "3"))
    # WITHVALUES / WITHSCORES: full-set pairs (sorted byte-exact) + negative cardinality
    def pairs_sorted(srv, *c):
        r = parr(cmd(srv, *c))
        if not isinstance(r, list):
            return r
        return tuple(sorted((r[i], r[i + 1]) for i in range(0, len(r), 2)))

    eq("hrand_withvalues_full", pairs_sorted(od, "HRANDFIELD", "h", "5", "WITHVALUES"),
       pairs_sorted(fr, "HRANDFIELD", "h", "5", "WITHVALUES"))
    eq("zrand_withscores_full", pairs_sorted(od, "ZRANDMEMBER", "z", "5", "WITHSCORES"),
       pairs_sorted(fr, "ZRANDMEMBER", "z", "5", "WITHSCORES"))
    eq("hrand_neg_wv_card", card(od, "HRANDFIELD", "h", "-4", "WITHVALUES"),
       card(fr, "HRANDFIELD", "h", "-4", "WITHVALUES"))
    # SRANDMEMBER has no WITHVALUES -> error (same shape)
    eq("srand_withvalues_err", cmd(od, "SRANDMEMBER", "s", "3", "WITHVALUES"),
       cmd(fr, "SRANDMEMBER", "s", "3", "WITHVALUES"))

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} RANDMEMBER-family divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — SRANDMEMBER/HRANDFIELD/ZRANDMEMBER count semantics match redis 7.2.4 "
        "(overflow=full-set, pos/neg cardinality, count-0, missing, WITH*, errors)"
    )


if __name__ == "__main__":
    main()
