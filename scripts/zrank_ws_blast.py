#!/usr/bin/env python3
"""Fixed-count ZRANK WITHSCORE blast for server-side perf-stat instruction A/B.
Usage: zrank_ws_blast.py PORT N [setup|blast]
  setup: load the 2000-member zset and exit (do once before perf window)
  blast: send N pipelined ZRANK ... WITHSCORE commands, read all replies, exit.
Run under: perf stat -e instructions:u -p <server_pid> -- python3 zrank_ws_blast.py PORT N blast
so perf measures exactly the server work for N commands."""
import socket, sys

PORT = int(sys.argv[1]); N = int(sys.argv[2]); MODE = sys.argv[3] if len(sys.argv) > 3 else "blast"


def enc(parts):
    out = [b"*%d\r\n" % len(parts)]
    for p in parts:
        p = str(p).encode() if not isinstance(p, bytes) else p
        out += [b"$%d\r\n" % len(p), p, b"\r\n"]
    return b"".join(out)


s = socket.create_connection(("127.0.0.1", PORT), 5)
s.settimeout(60)
buf = b""


def read_n(n):
    global buf
    got = 0
    while got < n:
        while b"\r\n" not in buf:
            buf += s.recv(1 << 20)
        nl = buf.find(b"\r\n"); line = buf[:nl]; buf = buf[nl + 2:]
        t = line[:1]
        if t == b"*":
            cnt = int(line[1:])
            if cnt > 0:
                read_n(cnt)  # nested elements counted separately below
                # the nested read consumed cnt replies; this array == 1 logical reply
            got += 1
        elif t in (b"$", b"="):
            ln = int(line[1:])
            if ln >= 0:
                while len(buf) < ln + 2:
                    buf += s.recv(1 << 20)
                buf = buf[ln + 2:]
            got += 1
        else:
            got += 1


if MODE == "setup":
    members = [x for j in range(2000) for x in (j, f"m{j}")]
    s.sendall(enc(["DEL", "z"]))
    s.sendall(enc(["ZADD", "z"] + members))
    read_n(2)
    sys.exit(0)

# blast: pipeline N ZRANK z m1000 WITHSCORE in chunks
CHUNK = 1000
one = enc(["ZRANK", "z", "m1000", "WITHSCORE"])
sent = 0
while sent < N:
    k = min(CHUNK, N - sent)
    s.sendall(one * k)
    # each reply is an array of 2 (integer + score) → read_n handles nested
    read_n(k)
    sent += k
