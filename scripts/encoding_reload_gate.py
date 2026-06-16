#!/usr/bin/env python3
"""Differential gate: OBJECT ENCODING + content must survive an RDB round-trip.

A value's reported encoding (listpack / hashtable / intset / skiplist / quicklist
/ int / embstr / raw) is part of fr's observable parity, and it must be the SAME
after DEBUG RELOAD as before — for the same data redis reports the same encoding
both live and reloaded. This invariant is easy to break in the RDB
save/reconstruct path: frankenredis-61e3p is exactly such a bug (a near-
`list-max-listpack-size` list reloads as `listpack` where redis keeps it
`quicklist`). This gate pins the encoding AND the DEBUG DIGEST-VALUE content of a
matrix of boundary values across string/hash/set/zset/list against vendored redis
7.2.4, both LIVE and after DEBUG RELOAD.

The list TOTAL-BYTE size boundary (~8 KB single listpack) is deliberately not
probed here — that is frankenredis-61e3p, tracked + being fixed separately;
list_chunk_seal_gate.py covers the seal, and lists here use only count-based and
clearly-large cases.

Usage: encoding_reload_gate.py <oracle_port> <fr_port>   (oracle = vendored redis)
"""
import socket
import sys


def _read_reply(s):
    data = bytearray()

    def read_line():
        line = bytearray()
        while not line.endswith(b"\r\n"):
            ch = s.recv(1)
            if not ch:
                break
            line += ch
        return bytes(line)

    def one():
        line = read_line()
        data.extend(line)
        if not line:
            return
        t = line[:1]
        if t in (b"+", b"-", b":", b"_", b"#", b",", b"("):
            return
        if t in (b"$", b"="):
            n = int(line[1:-2])
            if n < 0:
                return
            body = b""
            while len(body) < n + 2:
                body += s.recv(n + 2 - len(body))
            data.extend(body)
            return
        if t in (b"*", b"~", b">", b"%"):
            n = int(line[1:-2])
            if n < 0:
                return
            if t == b"%":
                n *= 2
            for _ in range(n):
                one()

    one()
    return bytes(data)


def send(s, *args):
    buf = b"*%d\r\n" % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        buf += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(buf)
    return _read_reply(s)


def build(s):
    send(s, "FLUSHALL")
    big = "x" * 65          # > *-max-listpack-value (64)
    small = "v"
    # strings: int (shared + non-shared), embstr, raw (appended)
    send(s, "SET", "str_int_shared", "100")
    send(s, "SET", "str_int_big", "123456789012345")
    send(s, "SET", "str_embstr", "short string under 44 bytes")
    send(s, "SET", "str_raw", "y" * 50)
    send(s, "SET", "str_appended", "abc")
    send(s, "APPEND", "str_appended", "def")          # APPEND forces raw
    # hashes: listpack (count boundary), hashtable (by count + by value)
    for i in range(128):
        send(s, "HSET", "h_lp", f"f{i}", small)
    for i in range(200):
        send(s, "HSET", "h_ht_count", f"f{i}", small)
    send(s, "HSET", "h_ht_value", "f", big)
    # sets: intset, intset->hashtable by count, listpack (strings), hashtable
    for i in range(128):
        send(s, "SADD", "set_intset", str(i))
    for i in range(600):
        send(s, "SADD", "set_int_ht", str(i))          # > set-max-intset-entries
    for i in range(64):
        send(s, "SADD", "set_lp", f"m{i}")
    for i in range(200):
        send(s, "SADD", "set_ht_count", f"m{i}")
    send(s, "SADD", "set_ht_value", big)
    # zsets: listpack, skiplist by count, skiplist by value
    for i in range(128):
        send(s, "ZADD", "z_lp", str(i), f"m{i}")
    for i in range(200):
        send(s, "ZADD", "z_sl_count", str(i), f"m{i}")
    send(s, "ZADD", "z_sl_value", "1", big)
    # lists: listpack (small) and quicklist (clearly large, far over 8KB)
    for i in range(20):
        send(s, "RPUSH", "l_lp", f"e{i}")
    for i in range(300):
        send(s, "RPUSH", "l_ql", "p" * 50)             # ~15 KB -> unambiguous quicklist


KEYS = [
    "str_int_shared", "str_int_big", "str_embstr", "str_raw", "str_appended",
    "h_lp", "h_ht_count", "h_ht_value",
    "set_intset", "set_int_ht", "set_lp", "set_ht_count", "set_ht_value",
    "z_lp", "z_sl_count", "z_sl_value",
    "l_lp", "l_ql",
]


def run(oracle, fr):
    diffs = 0
    for c in (oracle, fr):
        build(c)
    for phase, reload_first in (("live", False), ("post-RELOAD", True)):
        if reload_first:
            for c in (oracle, fr):
                send(c, "DEBUG", "RELOAD")
        for k in KEYS:
            for cmd in (("OBJECT", "ENCODING", k), ("DEBUG", "DIGEST-VALUE", k)):
                ro, rf = send(oracle, *cmd), send(fr, *cmd)
                if ro != rf:
                    diffs += 1
                    print(f"DIFF {phase} [{' '.join(cmd)}]\n  redis={ro!r}\n  fr   ={rf!r}")
    if diffs == 0:
        print(
            f"PASS — encoding + content survive RDB round-trip byte-exact vs "
            f"redis 7.2.4 ({len(KEYS)} keys x OBJECT ENCODING + DIGEST x2 passes)"
        )
    else:
        print(f"FAIL — {diffs} divergence(s)")
    return 1 if diffs else 0


def main():
    if len(sys.argv) < 3:
        print("usage: encoding_reload_gate.py <oracle_port> <fr_port>", file=sys.stderr)
        return 2
    oracle = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=10)
    fr = socket.create_connection(("127.0.0.1", int(sys.argv[2])), timeout=10)
    oracle.settimeout(10)
    fr.settimeout(10)
    try:
        return run(oracle, fr)
    finally:
        oracle.close()
        fr.close()


if __name__ == "__main__":
    sys.exit(main())
