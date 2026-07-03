#!/usr/bin/env python3
"""Fixed-count SET blast for server-side perf-stat instruction A/B.
Usage: set_blast.py PORT N [reset]
  reset: DEL the key and exit (UNMEASURED setup)
  else : send N pipelined `SET k v` (overwrite, no TTL) and read replies.
The key carries NO TTL; on a DB with no volatile keys expires_count==0, so the
candidate takes the contains_key branch and skips drop_if_expired's expiry-map probe.
Both paths overwrite identically, so that cost cancels — the only delta is the
eliminated expiry_ms lookup on the set_plain_borrowed overwrite path."""
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
        got += 1  # SET replies are all +OK, one line each.


if MODE == "reset":
    s.sendall(enc(["DEL", "k"]))
    read_replies(1)
    sys.exit(0)

cmd = enc(["SET", "k", "somevalue123"])
sent = 0; CHUNK = 1000
while sent < N:
    k = min(CHUNK, N - sent)
    s.sendall(cmd * k)
    read_replies(k)
    sent += k
