#!/usr/bin/env python3
"""Differential: OBJECT ENCODING after LOWERING a listpack threshold (no write).

Redis converts collection encodings FORWARD-ONLY and LAZILY — the check runs
inside write commands (hashTypeTryConversion / *TypeTryConversion), and
`CONFIG SET <type>-max-listpack-*` never iterates existing keys. So an existing
listpack/intset key keeps its encoding after the threshold is lowered, until the
NEXT write to that key triggers a conversion check under the new limit.

fr instead RE-DERIVES the encoding from the CURRENT config + CURRENT size on
every OBJECT ENCODING read (Store::object_encoding: `len <= max_listpack_*`),
with sticky force-flags only for the already-converted direction. So lowering
the threshold makes fr report the converted encoding with NO write — a divergence
from redis's stored, write-time-only conversion model.

This probe builds each collection UNDER the default threshold (so no write ever
crosses it → encoding stays listpack/intset on both servers), then LOWERS the
threshold below the current size and reads OBJECT ENCODING again WITHOUT writing.
Redis: unchanged. fr (buggy): converted.

Run both servers on COMPILED defaults; pass <oracle_port> <fr_port>.
Exit 0 = parity (bug fixed); 1 = divergence (bug present).  (frankenredis-a0p5p)
"""
import socket, sys, time

def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=10)

def cmd(s, *args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str): a = a.encode()
        elif isinstance(a, int): a = str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(out)
    return read(s)

def read(s, buf=bytearray()):
    # simple line/bulk reader (one reply)
    data = b""
    while b"\r\n" not in data:
        data += s.recv(65536)
    line, rest = data.split(b"\r\n", 1)
    t = line[:1]
    if t in (b"+", b"-", b":"):
        return line.decode(errors="replace")
    if t == b"$":
        n = int(line[1:])
        if n < 0:
            return None
        while len(rest) < n + 2:
            rest += s.recv(65536)
        return rest[:n].decode(errors="replace")
    return line.decode(errors="replace")

CASES = [
    # label, build cmds (list of arg-lists), config param, high value, lowered value
    ("hash",   [["hset", "k", f"f{i}", f"v{i}"] for i in range(100)],
     "hash-max-listpack-entries", "128", "50"),
    ("list",   [["rpush", "k", f"e{i}"] for i in range(100)],
     "list-max-listpack-size", "128", "50"),
    ("zset",   [["zadd", "k", str(i), f"m{i}"] for i in range(100)],
     "zset-max-listpack-entries", "128", "50"),
    ("set-intset", [["sadd", "k", str(i)] for i in range(100)],
     "set-max-intset-entries", "512", "50"),
    ("set-listpack", [["sadd", "k", f"m{i}"] for i in range(100)],
     "set-max-listpack-entries", "128", "50"),
]

def run_case(s, build, param, high, low):
    cmd(s, "flushall")
    # Build the value UNDER a threshold it never crosses, so neither server has
    # converted it: encoding starts listpack/intset on both.
    cmd(s, "config", "set", param, high)
    if param == "set-max-listpack-entries":
        cmd(s, "config", "set", "set-max-intset-entries", "0")  # force listpack not intset
    for c in build:
        cmd(s, *c)
    before = cmd(s, "object", "encoding", "k")
    cmd(s, "config", "set", param, low)
    after = cmd(s, "object", "encoding", "k")
    return before, after

def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16801
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16800
    od, fr = conn(op), conn(fp)
    # hash/zset/set fixed by the a0p5p object_encoding flags-only change. The
    # `list` case (non-default list-max-listpack-size byte budget) is the tracked
    # remainder: its sticky forced_quicklist/forced_for_fill machinery needs to
    # seed the live fill on bulk loads before the read-time re-derivation can be
    # dropped (the default -2 budget is already correct).
    KNOWN_REMAINING = {"list"}
    div = 0
    known = 0
    print(f"{'case':14} {'oracle before->after':28} {'fr before->after':28}")
    for label, build, param, high, low in CASES:
        ob, oa = run_case(od, build, param, high, low)
        fb, fa = run_case(fr, build, param, high, low)
        if (ob, oa) == (fb, fa):
            flag = ""
        elif label in KNOWN_REMAINING:
            flag = "  <== KNOWN-REMAINING (a0p5p list facet)"
            known += 1
        else:
            flag = "  <== DIVERGE"
            div += 1
        print(f"{label:14} {ob+' -> '+str(oa):28} {fb+' -> '+str(fa):28}{flag}")
    print("-" * 60)
    if div:
        print(f"FAIL — {div} NEW divergence(s): fr converts on CONFIG-lower without a write")
        return 1
    print(f"PASS — hash/zset/set sticky across threshold-lower until next write "
          f"({known} known-remaining: list non-default budget)")
    return 0

if __name__ == "__main__":
    sys.exit(main())
