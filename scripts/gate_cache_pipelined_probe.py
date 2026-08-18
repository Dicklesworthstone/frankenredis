#!/usr/bin/env python3
"""Pipelined stale-read-gate probe for the FastReply cache-clear removal.

The dispatch_route_differ is REQUEST-RESPONSE: one command per buffered pass, so the
read-gate cache is always fresh and the corpus cannot see this hazard at all. The cache
only persists WITHIN one buffered pass, so the only way a stale gate can be observed is to
pipeline a gate-INVALIDATING command and a borrowed read in a SINGLE write().

SELECT 1 makes the gate false (`selected_db != 0`). If the cached answer survived it, the
borrowed fast path would run against db 0 and return db 0's value where the correct answer
is db 1's. So the observable is a WRONG REPLY, not a crash.
"""
import socket, sys

def enc(*a):
    out = [b"*%d\r\n" % len(a)]
    for x in a:
        b = x.encode() if isinstance(x, str) else x
        out.append(b"$%d\r\n" % len(b) + b + b"\r\n")
    return b"".join(out)

def run(port, batch):
    s = socket.create_connection(("127.0.0.1", port), 5)
    s.sendall(b"".join(enc(*c) for c in batch))     # ONE write => one buffered pass
    s.settimeout(5)
    buf = b""
    # read until we have at least as many CRLF-terminated replies as commands
    while buf.count(b"\r\n") < len(batch):
        chunk = s.recv(1 << 16)
        if not chunk:
            break
        buf += chunk
    s.close()
    return buf

BATCHES = {
    # the load-bearing one: SELECT flips the gate mid-pass, then borrowed reads follow
    "select_then_reads": [
        ("SELECT", "0"), ("RPUSH", "gp:l", "a", "b", "c"), ("SET", "gp:s", "hello"),
        ("LLEN", "gp:l"), ("STRLEN", "gp:s"),
        ("SELECT", "1"),
        ("LLEN", "gp:l"), ("STRLEN", "gp:s"), ("SCARD", "gp:l"),
        ("SELECT", "0"),
        ("LLEN", "gp:l"), ("STRLEN", "gp:s"),
    ],
    # a read-only batch, to show the probe is not simply always divergent
    "reads_only": [
        ("SELECT", "0"), ("RPUSH", "gp:m", "x", "y"),
        ("LLEN", "gp:m"), ("LLEN", "gp:m"), ("LLEN", "gp:m"),
    ],
}

fr, rd = int(sys.argv[1]), int(sys.argv[2])
bad = 0
for name, batch in BATCHES.items():
    a, b = run(fr, batch), run(rd, batch)
    ok = a == b
    bad += 0 if ok else 1
    print(f"  {name:20s} {'MATCH' if ok else 'DIVERGES'}")
    if not ok:
        print(f"      fr    {a!r}")
        print(f"      redis {b!r}")
print(f"{len(BATCHES)} pipelined batches, {bad} divergence(s)")
sys.exit(0 if bad == 0 else 1)
