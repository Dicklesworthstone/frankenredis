#!/usr/bin/env python3
"""Differential gate: OBJECT ENCODING transitions under RUNTIME `CONFIG SET`.

The existing encoding_differ.py exercises encoding only at the COMPILED defaults.
This gate complements it by driving the live config->encoding recomputation path:
it CONFIG SETs the *-max-listpack-*/intset thresholds to small NON-default values
(identically on both servers, so it dodges the config-default false-positive
class) and walks each collection type across its entry-count and value-length
boundaries, asserting fr's OBJECT ENCODING matches vendored redis 7.2.4 byte for
byte at every step.

This surface has historically harbored real bugs (gdep4 list SIZE_SAFETY_LIMIT,
hpfey config-reset-on-reload, B2 intset integer-overflow conversion), none of
which a defaults-only probe can reach.

SETUP (both servers config-LESS so only the CONFIG SETs below differ from
compiled defaults):
  ORACLE=legacy_redis_code/redis/src
  $ORACLE/redis-server --port 17821 --daemonize yes --save '' --appendonly no
  $CARGO_TARGET_DIR/debug/frankenredis --port 17822 --mode strict &
  scripts/encoding_config_boundary_differ.py --oracle 17821 --fr 17822

Exit status: 0 = byte-exact, 1 = at least one divergence (details printed).
"""
import argparse
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.buf = b""

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            a = str(a).encode() if not isinstance(a, bytes) else a
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self._read()

    def _read(self):
        while b"\r\n" not in self.buf:
            self.buf += self.s.recv(65536)
        line, self.buf = self.buf.split(b"\r\n", 1)
        t, rest = line[:1], line[1:]
        if t in (b"+", b"-", b":"):
            return line
        if t == b"$":
            n = int(rest)
            if n < 0:
                return b"$-1"
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(65536)
            d = self.buf[:n]
            self.buf = self.buf[n + 2:]
            return b"$" + d
        if t == b"*":
            n = int(rest)
            if n < 0:
                return line
            return line + b"|" + b"|".join(self._read() for _ in range(n))
        return line


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    o = Conn(args.oracle)
    f = Conn(args.fr)

    diffs = 0

    def both(*a):
        o.cmd(*a)
        f.cmd(*a)

    def check(*a):
        nonlocal diffs
        ro, rf = o.cmd(*a), f.cmd(*a)
        if ro != rf:
            diffs += 1
            print(f"DIVERGE {a}\n  oracle={ro!r}\n  fr    ={rf!r}")

    # Small non-default thresholds, set identically on both servers.
    for kv in [
        ("set-max-intset-entries", "4"),
        ("set-max-listpack-entries", "4"),
        ("set-max-listpack-value", "8"),
        ("hash-max-listpack-entries", "4"),
        ("hash-max-listpack-value", "8"),
        ("zset-max-listpack-entries", "4"),
        ("zset-max-listpack-value", "8"),
        ("list-max-listpack-size", "4"),
    ]:
        both("CONFIG", "SET", *kv)
    both("FLUSHALL")

    # Set: integer members crossing intset->listpack->hashtable boundaries.
    for n in range(1, 9):
        both("DEL", "si")
        for i in range(n):
            both("SADD", "si", i)
        check("OBJECT", "ENCODING", "si")
    # Set: non-integer members crossing listpack->hashtable.
    for n in range(1, 9):
        both("DEL", "ss")
        for i in range(n):
            both("SADD", "ss", f"m{i}")
        check("OBJECT", "ENCODING", "ss")
    # Set: a member longer than set-max-listpack-value forces hashtable.
    both("DEL", "sv")
    both("SADD", "sv", "x" * 12)
    check("OBJECT", "ENCODING", "sv")
    # Set: integer overflow of set-max-intset-entries -> hashtable (B2).
    both("DEL", "sov")
    for i in range(6):
        both("SADD", "sov", i)
    check("OBJECT", "ENCODING", "sov")

    # Hash: entry-count and value-length boundaries.
    for n in range(1, 9):
        both("DEL", "h")
        for i in range(n):
            both("HSET", "h", f"f{i}", f"v{i}")
        check("OBJECT", "ENCODING", "h")
    both("DEL", "hv")
    both("HSET", "hv", "f", "x" * 12)
    check("OBJECT", "ENCODING", "hv")

    # Zset: entry-count and value-length boundaries.
    for n in range(1, 9):
        both("DEL", "z")
        for i in range(n):
            both("ZADD", "z", i, f"m{i}")
        check("OBJECT", "ENCODING", "z")
    both("DEL", "zv")
    both("ZADD", "zv", 1, "x" * 12)
    check("OBJECT", "ENCODING", "zv")

    # List: entry-count and element-size boundaries (listpack<->quicklist).
    for n in range(1, 9):
        both("DEL", "l")
        for i in range(n):
            both("RPUSH", "l", f"e{i}")
        check("OBJECT", "ENCODING", "l")
    both("DEL", "lv")
    both("RPUSH", "lv", "x" * 80)
    check("OBJECT", "ENCODING", "lv")

    if diffs:
        print(f"\nFAIL: {diffs} encoding divergence(s) under runtime CONFIG SET")
        sys.exit(1)
    print("OK: OBJECT ENCODING byte-exact across runtime CONFIG SET boundaries "
          "vs redis 7.2.4 (set/hash/zset/list entry-count + value-length + intset overflow)")


if __name__ == "__main__":
    main()
