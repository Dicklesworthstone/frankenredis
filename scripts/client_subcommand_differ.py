#!/usr/bin/env python3
"""Differential gate: CLIENT subcommand deterministic edges (frankenredis-del6l).

CLIENT has many subcommands whose replies are connection-independent and thus
byte-exact-comparable: GETNAME (empty by default), SETNAME (rejects names with
spaces or newlines, allows empty=clear, per-connection), NO-EVICT / NO-TOUCH
(ON/OFF + bad-arg syntax error), REPLY ON, unknown-subcommand error, HELP (a fixed
array). CLIENT ID is a per-connection monotonic integer, so its VALUE differs — we
only assert both return an integer reply, not the number. CLIENT INFO/LIST are
per-connection (addr/fd/id) and intentionally not compared here.

Uses one persistent connection per server so SETNAME->GETNAME observe the same
client.

Usage: client_subcommand_differ.py <oracle_port> <fr_port>
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


CASES = [
    ["CLIENT", "GETNAME"],                       # empty by default
    ["CLIENT", "SETNAME", "myconn"],
    ["CLIENT", "GETNAME"],                        # myconn
    ["CLIENT", "SETNAME", "has space"],           # err: no spaces
    ["CLIENT", "SETNAME", "has\nnl"],             # err: no newlines
    ["CLIENT", "SETNAME", ""],                    # ok (clears)
    ["CLIENT", "GETNAME"],                         # empty again
    ["CLIENT", "SETNAME", "a", "b"],              # err arity
    ["CLIENT", "NO-EVICT", "ON"],
    ["CLIENT", "NO-EVICT", "OFF"],
    ["CLIENT", "NO-EVICT", "MAYBE"],              # err syntax
    ["CLIENT", "NO-TOUCH", "ON"],
    ["CLIENT", "NO-TOUCH", "OFF"],
    ["CLIENT", "NO-TOUCH", "x"],                  # err syntax
    ["CLIENT", "REPLY", "ON"],                    # +OK
    ["CLIENT", "BOGUS"],                          # err unknown subcommand
    ["client", "getname"],                        # case-insensitive
    ["CLIENT", "HELP"],                           # fixed help array
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    for argv in CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{' '.join(argv)!r}: redis={ro!r} fr={rf!r}")
    # CLIENT ID: per-connection value differs; assert both return a positive integer.
    io, i_f = cmd(od, "CLIENT", "ID"), cmd(fr, "CLIENT", "ID")
    for tag, r in (("redis", io), ("fr", i_f)):
        if not (r.startswith(b":") and int(r[1 : r.index(b"\r\n")]) > 0):
            fails.append(f"CLIENT ID {tag} not a positive integer: {r!r}")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} CLIENT subcommand divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — CLIENT subcommands byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases: GETNAME/SETNAME-validation/NO-EVICT/NO-TOUCH/REPLY/HELP/errors + ID integer-shape)"
    )


if __name__ == "__main__":
    main()
