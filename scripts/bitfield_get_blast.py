#!/usr/bin/env python3
"""Fixed-count read-only BITFIELD (multi-GET) blast for server-side perf-stat A/B.
Usage: bitfield_get_blast.py PORT N [setup]
  setup: SET the key to a 64-byte string and exit (UNMEASURED setup)
  else : send N pipelined `BITFIELD k GET u8 0 GET u8 8 ... (8 GETs)` and read replies.
Candidate collapses the per-op drop_if_expired+entries.get (~3 keyspace lookups/op)
into ONE lookup for the whole command; control re-resolves the key on every GET op.
The 8-GET shape amplifies the per-op saving so the instruction delta clears noise."""
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
        # each reply is an 8-element array of integers; just drain by counting the
        # outer array header then its 8 elements. Simplest: count total lines.
        nl = buf.find(b"\r\n"); buf = buf[nl + 2:]
        got += 1


if MODE == "setup":
    s.sendall(enc(["SET", "k", "0123456789" * 6 + "0123"]))  # 64 bytes
    read_replies(1)
    sys.exit(0)

# 8 GET ops over the 64-byte value.
parts = ["BITFIELD", "k"]
for i in range(8):
    parts += ["GET", "u8", i * 8]
cmd = enc(parts)

# Each reply: outer array header (1 line) + 8 integer lines = 9 lines per command.
LINES_PER = 9
sent = 0; CHUNK = 500
while sent < N:
    k = min(CHUNK, N - sent)
    s.sendall(cmd * k)
    read_replies(k * LINES_PER)
    sent += k
