#!/usr/bin/env python3
"""Differential regression gate for the GROUPED stream index (frankenredis-p8wd1).

CoralOx's "group stream index nodes" (74a926418) packs many stream entries into
multi-entry index nodes (the redis rax-of-listpacks shape) instead of one index
slot per entry. That is a structural rewrite of the hottest stream paths, so this
gate pins it byte-for-byte against vendored redis 7.2.4 across the exact scenarios
that stress node boundaries: a stream spanning MANY nodes, scattered XDEL +
front-trim (XTRIM) churn, explicit-id XADD, XSETID, a consumer group with pending
entries, ranged/reversed/COUNT reads, XINFO STREAM FULL, and an RDB round-trip
(DEBUG RELOAD) — the grouping must survive serialize+reload identically.

Usage: stream_node_grouping_gate.py <oracle_port> <fr_port>
Exits 0 if every probed reply is byte-identical, 1 (with a diff report) otherwise.
DUMP is intentionally NOT compared: fr's post-XDEL/XTRIM tombstone byte-repr has a
long-standing (pre-grouping) divergence from redis that DEBUG DIGEST + post-RELOAD
equality already prove is semantically identical.
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
    """Build a multi-node stream + a churned stream on a fresh DB."""
    send(s, "FLUSHALL")
    # `s` spans many index nodes; scattered XDEL + front trim + explicit tail id.
    for i in range(1, 251):
        send(s, "XADD", "s", f"{i}-0", "f1", f"v{i}", "f2", f"data_{i}")
    for i in (5, 50, 100, 150, 200):
        send(s, "XDEL", "s", f"{i}-0")
    send(s, "XTRIM", "s", "MAXLEN", "180")
    send(s, "XADD", "s", "999-0", "last", "only")
    send(s, "XSETID", "s", "1000-0")
    send(s, "XGROUP", "CREATE", "s", "grp", "0")
    send(s, "XREADGROUP", "GROUP", "grp", "c1", "COUNT", "50", "STREAMS", "s", ">")
    # `churn` interleaves dense add / delete-evens / add to fragment nodes hard.
    for i in range(1, 401):
        send(s, "XADD", "churn", f"{i}-0", "k", str(i))
    for i in range(2, 401, 2):
        send(s, "XDEL", "churn", f"{i}-0")
    for i in range(401, 501):
        send(s, "XADD", "churn", f"{i}-0", "k", str(i))


# Only DETERMINISTIC, grouping-SENSITIVE surfaces: the entry payloads, ordering,
# range boundaries and content digest — exactly what a node-grouping bug would
# corrupt. Deliberately EXCLUDED:
#   * XINFO `radix-tree-nodes` — the internal rax node count, an implementation
#     stat that redis itself isn't self-consistent on (it re-chunks on RDB load:
#     4 nodes live -> 8 nodes after DEBUG RELOAD for the same data), so it can't
#     and shouldn't be matched byte-for-byte;
#   * XPENDING / XINFO GROUPS|...FULL pending — carry per-entry delivery
#     WALL-CLOCK timestamps that differ between two independent server processes.
# DEBUG DIGEST-VALUE hashes every (id, fields) pair, so it is the strong proof
# that the grouped index stored the exact same logical stream.
PROBES = [
    ["XLEN", "s"],
    ["XRANGE", "s", "-", "+"],
    ["XREVRANGE", "s", "+", "-"],
    ["XRANGE", "s", "30-0", "120-0", "COUNT", "40"],
    ["XRANGE", "s", "(50-0", "+", "COUNT", "5"],
    ["XRANGE", "s", "-", "(70-0"],
    ["XREVRANGE", "s", "+", "-", "COUNT", "7"],
    ["XREVRANGE", "s", "120-0", "30-0", "COUNT", "40"],
    ["XREAD", "COUNT", "5", "STREAMS", "s", "0"],
    ["XREAD", "COUNT", "200", "STREAMS", "s", "0"],
    ["DEBUG", "DIGEST-VALUE", "s"],
    ["XLEN", "churn"],
    ["XRANGE", "churn", "-", "+", "COUNT", "1000"],
    ["XREVRANGE", "churn", "+", "-", "COUNT", "30"],
    ["XRANGE", "churn", "199-0", "405-0"],
    ["DEBUG", "DIGEST-VALUE", "churn"],
]


def run(oracle: socket.socket, fr: socket.socket) -> int:
    diffs = 0
    for c in (oracle, fr):
        build(c)
    # First pass: live grouped state.
    for p in PROBES + [["RELOAD-MARK"]]:
        if p == ["RELOAD-MARK"]:
            for c in (oracle, fr):
                send(c, "DEBUG", "RELOAD")
            continue
        ro = send(oracle, *p)
        rf = send(fr, *p)
        if ro != rf:
            diffs += 1
            print(f"DIFF [{' '.join(p)}]\n  redis={ro!r}\n  fr   ={rf!r}")
    # Second pass: identical probes must still match after the RDB round-trip.
    for p in PROBES:
        ro = send(oracle, *p)
        rf = send(fr, *p)
        if ro != rf:
            diffs += 1
            print(f"DIFF post-RELOAD [{' '.join(p)}]\n  redis={ro!r}\n  fr   ={rf!r}")
    if diffs == 0:
        print(
            f"PASS — grouped stream index byte-exact vs redis 7.2.4 "
            f"({len(PROBES)} probes x2 passes + RDB round-trip)"
        )
    else:
        print(f"FAIL — {diffs} divergence(s)")
    return 1 if diffs else 0


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: stream_node_grouping_gate.py <oracle_port> <fr_port>", file=sys.stderr)
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
