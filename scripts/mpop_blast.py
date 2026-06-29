#!/usr/bin/env python3
"""Fixed-count LMPOP/ZMPOP blast for server-side perf-stat instruction A/B.
Usage:
  mpop_blast.py PORT N preload_list|preload_zset   # load key K with N+slack elems (UNMEASURED)
  mpop_blast.py PORT N lmpop_nocount|lmpop_count1|zmpop_nocount|zmpop_count1  # blast only
Run the blast under: perf stat -e instructions:u -p <server_pid> -- python3 mpop_blast.py PORT N MODE"""
import socket, sys

PORT = int(sys.argv[1]); N = int(sys.argv[2]); MODE = sys.argv[3]


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
        t = line[:1]
        if t == b"*":
            cnt = int(line[1:])
            if cnt > 0:
                read_replies(cnt)
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


if MODE in ("preload_list", "preload_zset"):
    SLACK = N + 20000
    s.sendall(enc(["DEL", "K"])); read_replies(1)
    CH = 5000; loaded = 0
    while loaded < SLACK:
        k = min(CH, SLACK - loaded)
        if MODE == "preload_zset":
            members = [x for j in range(loaded, loaded + k) for x in (j, f"m{j}")]
            s.sendall(enc(["ZADD", "K"] + members))
        else:
            s.sendall(enc(["RPUSH", "K"] + [f"e{j}" for j in range(loaded, loaded + k)]))
        read_replies(1)
        loaded += k
    sys.exit(0)

CMDS = {
    "lmpop_nocount": ["LMPOP", "1", "K", "LEFT"],
    "lmpop_count1": ["LMPOP", "1", "K", "LEFT", "COUNT", "1"],
    "zmpop_nocount": ["ZMPOP", "1", "K", "MIN"],
    "zmpop_count1": ["ZMPOP", "1", "K", "MIN", "COUNT", "1"],
}
cmd = enc(CMDS[MODE])
sent = 0; CHUNK = 1000
while sent < N:
    k = min(CHUNK, N - sent)
    s.sendall(cmd * k)
    read_replies(k)
    sent += k
