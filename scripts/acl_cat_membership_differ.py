#!/usr/bin/env python3
"""Differential gate for ACL CAT per-category command MEMBERSHIP, fr vs redis 7.2.4.

`ACL CAT <category>` lists every command in an ACL category (@read, @write,
@dangerous, string, list, ...). The ORDER is dict-iteration order (a documented
WONTFIX, like FUNCTION LIST), but the MEMBERSHIP — which commands belong to each
category — is a hard correctness property: ACL +@read must grant exactly redis's
@read set, and a miscategorized/omitted command is a real ACL security bug.

acl_semantics_gate.py compares ACL CAT and ACL CAT read/write (sorted). This
gate enumerates EVERY category from `ACL CAT` and compares each category's
command list as an order-independent SET vs redis 7.2.4, so a categorization
drift in ANY category (not just read/write) is caught.

Usage: acl_cat_membership_differ.py <oracle_port> <fr_port>
       Exit 0 = every category's membership matches, 1 = divergence.
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
    time.sleep(0.03)
    return s.recv(1 << 20)


def flat_bulk_array(b):
    """Parse a flat RESP array of bulk strings into a list of str."""
    out = []
    nl = b.index(b"\r\n")
    if b[:1] != b"*":
        return out
    i = nl + 2
    while i < len(b):
        if b[i:i + 1] != b"$":
            break
        j = b.index(b"\r\n", i)
        n = int(b[i + 1:j])
        if n < 0:
            i = j + 2
            continue
        out.append(b[j + 2:j + 2 + n].decode("latin1"))
        i = j + 2 + n + 2
    return out


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)

    cats_o = set(flat_bulk_array(cmd(od, "ACL", "CAT")))
    cats_f = set(flat_bulk_array(cmd(fr, "ACL", "CAT")))
    fails = []
    if cats_o != cats_f:
        fails.append(
            f"top-level category set differs: redis-only={sorted(cats_o - cats_f)} "
            f"fr-only={sorted(cats_f - cats_o)}"
        )
    shared = sorted(cats_o & cats_f)
    for c in shared:
        so = set(flat_bulk_array(cmd(od, "ACL", "CAT", c)))
        sf = set(flat_bulk_array(cmd(fr, "ACL", "CAT", c)))
        if so != sf:
            fails.append(
                f"@{c}: redis-only={sorted(so - sf)} fr-only={sorted(sf - so)}"
            )

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} ACL category membership divergence(s) vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — all {len(shared)} ACL categories have byte-exact command membership "
        "vs redis 7.2.4 (order WONTFIX-normalized)"
    )


if __name__ == "__main__":
    main()
