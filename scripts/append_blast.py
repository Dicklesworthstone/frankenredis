#!/usr/bin/env python3
"""Fixed-count APPEND blast for server-side perf-stat instruction A/B.
Usage: append_blast.py PORT N [reset]
  reset: DEL the key and exit (UNMEASURED setup)
  else : send N pipelined `APPEND k <5 bytes>` and read replies (perf wraps this).
Candidate and control grow the key identically, so the per-append growth cost cancels
in the A/B; the only delta is the eliminated string_len_no_stats lookup."""
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
        nl = buf.find(b"\r\n"); line = buf[:nl]; buf = buf[nl + 2:]
        got += 1  # APPEND replies are all integers (:len)


if MODE == "reset":
    s.sendall(enc(["DEL", "k"]))
    read_replies(1)
    sys.exit(0)

cmd = enc(["APPEND", "k", "abcde"])
sent = 0; CHUNK = 1000
while sent < N:
    k = min(CHUNK, N - sent)
    s.sendall(cmd * k)
    read_replies(k)
    sent += k
