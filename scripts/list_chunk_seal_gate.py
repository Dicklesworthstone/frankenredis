#!/usr/bin/env python3
"""Differential regression gate for SEALED quicklist chunks (frankenredis-99fwc).

CoralOx's "seal quicklist list chunks for DUMP" (8c2421045) converts a full
interior `Owned` list chunk (a `Vec<Vec<u8>>` — one heap block per element) into
the compact `Listpack` representation when a fresh chunk starts, mirroring redis's
quicklist-of-listpacks. That is a structural change to list storage with two
delicate behaviours this gate pins against vendored redis 7.2.4:

  1. SEALED-chunk reads + RE-MATERIALIZATION: a later in-place mutation
     (LSET / LINSERT / LREM) on a sealed (`Listpack`) chunk must transparently
     convert it back to `Owned` and stay byte-identical. We build lists that span
     many sealed nodes, then mutate / pop / re-push and compare every read
     surface (LLEN, LRANGE full/ranged/negative, LINDEX, LPOS, OBJECT ENCODING,
     DEBUG DIGEST-VALUE) — live AND after a DEBUG RELOAD round-trip.
  2. DUMP cross-impl compatibility: fr's quicklist DUMP bytes are not identical to
     redis's (different node-byte boundaries — a long-standing, benign repr
     difference), so we do NOT compare DUMP bytes; instead we RESTORE fr's DUMP
     payload INTO redis and assert the resulting list content equals redis's own.

Usage: list_chunk_seal_gate.py <oracle_port> <fr_port>   (oracle = vendored redis)
Exits 0 if every probe matches, 1 (with a report) otherwise.
"""
import socket
import sys


def _read_reply(s: socket.socket) -> bytes:
    data = bytearray()

    def read_line() -> bytes:
        line = bytearray()
        while not line.endswith(b"\r\n"):
            ch = s.recv(1)
            if not ch:
                break
            line += ch
        return bytes(line)

    def one() -> None:
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


def send(s: socket.socket, *args) -> bytes:
    buf = b"*%d\r\n" % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        buf += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(buf)
    return _read_reply(s)


def build(s: socket.socket) -> None:
    send(s, "FLUSHALL")
    # `L` spans many sealed nodes; then mutate sealed chunks + churn the ends.
    for i in range(1, 601):
        send(s, "RPUSH", "L", f"elem_value_{i}")
    send(s, "LSET", "L", "50", "SET_mid")
    send(s, "LINSERT", "L", "BEFORE", "elem_value_300", "INSERTED")
    send(s, "LREM", "L", "0", "elem_value_500")
    for _ in range(30):
        send(s, "LPOP", "L")
        send(s, "RPOP", "L")
    for i in range(1, 51):
        send(s, "LPUSH", "L", f"front_{i}")
    # `M`: a big element forces the node-byte limit (single-element packed node).
    send(s, "RPUSH", "M", "short", "x" * 5000, "short2")


PROBES = [
    ["LLEN", "L"],
    ["LRANGE", "L", "0", "-1"],
    ["LRANGE", "L", "100", "200"],
    ["LRANGE", "L", "-50", "-1"],
    ["LRANGE", "L", "0", "0"],
    ["LINDEX", "L", "0"],
    ["LINDEX", "L", "250"],
    ["LINDEX", "L", "-1"],
    ["LPOS", "L", "INSERTED"],
    ["LPOS", "L", "front_25"],
    ["LPOS", "L", "SET_mid", "COUNT", "0"],
    ["DEBUG", "DIGEST-VALUE", "L"],
    ["LLEN", "M"],
    ["LRANGE", "M", "0", "-1"],
]

# OBJECT ENCODING is checked LIVE only: it diverges post-DEBUG-RELOAD for a list
# that sits just over list-max-listpack-size (fr's RDB save consolidates it into
# one listpack node -> reports `listpack` where redis keeps it `quicklist`) — a
# PRE-EXISTING node-boundary issue (frankenredis-61e3p), unrelated to the seal,
# which this gate exists to guard. Live, the seal must NOT change the encoding.
LIVE_ONLY = [
    ["OBJECT", "ENCODING", "L"],
    ["OBJECT", "ENCODING", "M"],
]


def dump_payload(reply: bytes) -> bytes:
    """Extract the bulk payload from a `$<n>\\r\\n<payload>\\r\\n` DUMP reply."""
    assert reply[:1] == b"$", reply[:32]
    nl = reply.index(b"\r\n")
    n = int(reply[1:nl])
    return reply[nl + 2 : nl + 2 + n]


def run(oracle: socket.socket, fr: socket.socket) -> int:
    diffs = 0
    for c in (oracle, fr):
        build(c)
    for label, after_reload in (("live", False), ("post-RELOAD", True)):
        if after_reload:
            for c in (oracle, fr):
                send(c, "DEBUG", "RELOAD")
        probes = PROBES if after_reload else PROBES + LIVE_ONLY
        for p in probes:
            ro, rf = send(oracle, *p), send(fr, *p)
            if ro != rf:
                diffs += 1
                print(f"DIFF {label} [{' '.join(p)}]\n  redis={ro!r}\n  fr   ={rf!r}")
    # DUMP cross-impl: RESTORE fr's payload into redis, compare list content.
    for key in ("L", "M"):
        payload = dump_payload(send(fr, "DUMP", key))
        send(oracle, "DEL", f"{key}_fr")
        resp = send(oracle, "RESTORE", f"{key}_fr", "0", payload)
        if not resp.startswith(b"+OK"):
            diffs += 1
            print(f"DIFF dump-restore [{key}]: redis rejected fr's DUMP: {resp!r}")
            continue
        a = send(oracle, "LRANGE", f"{key}_fr", "0", "-1")
        b = send(oracle, "LRANGE", key, "0", "-1")
        if a != b:
            diffs += 1
            print(f"DIFF dump-restore content [{key}]: fr DUMP->redis RESTORE != redis")
    if diffs == 0:
        print(
            f"PASS — sealed quicklist chunks byte-exact vs redis 7.2.4 "
            f"({len(PROBES)} probes x2 passes + DUMP->RESTORE cross-impl on 2 keys)"
        )
    else:
        print(f"FAIL — {diffs} divergence(s)")
    return 1 if diffs else 0


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: list_chunk_seal_gate.py <oracle_port> <fr_port>", file=sys.stderr)
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
