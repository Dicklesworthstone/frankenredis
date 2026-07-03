#!/usr/bin/env python3
"""Fixed-count HSET blast for server-side perf-stat instruction A/B.
Usage: hset_blast.py PORT N [reset]
  reset: DEL the key and exit (UNMEASURED setup)
  else : send N pipelined `HSET k f0 v0 f1 v1 f2 v2 f3 v3` (4 fields) and read replies.
The key carries NO TTL, so on a DB with no volatile keys expires_count==0 and the
candidate skips the per-call drop_if_expired (2 keyspace lookups); control always runs
it. Both paths mutate the hash identically (same fields overwritten), so that cost
cancels — the only delta is the eliminated drop_if_expired lookup pair."""
import socket, sys

PORT = int(sys.argv[1]); N = int(sys.argv[2]); MODE = sys.argv[3] if len(sys.argv) > 3 else "blast"


def enc(parts):
    out = [b"*%d\r\n" % len(parts)]
    for p in parts:
        p = str(p).encode() if not isinstance(p, bytes) else p
        out += [b"$%d\r\n" % len(p), p, b"\r\n"]
    return b"".join(out)


s = socket.create_connection(("127.0.0.1", PORT), 5)
s.settimeout(120)
buf = b""


def read_replies(n):
    global buf
    got = 0
    while got < n:
        while b"\r\n" not in buf:
            buf += s.recv(1 << 20)
        nl = buf.find(b"\r\n"); buf = buf[nl + 2:]
        got += 1  # HSET replies are all :<int> integers, one line each.


if MODE == "reset":
    s.sendall(enc(["DEL", "k"]))
    read_replies(1)
    sys.exit(0)

cmd = enc(["HSET", "k", "f0", "v0", "f1", "v1", "f2", "v2", "f3", "v3"])
sent = 0; CHUNK = 1000
while sent < N:
    k = min(CHUNK, N - sent)
    s.sendall(cmd * k)
    read_replies(k)
    sent += k
