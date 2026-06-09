#!/usr/bin/env python3
"""Differential gate: OBJECT ENCODING after a RESTORE command matches redis 7.2.4.

dump_restore_fuzz.py proves the DUMP *payload bytes* round-trip identically, but
it does not assert that the value RESTORE rebuilds lands in the SAME OBJECT
ENCODING upstream would pick. RESTORE re-decodes a serialized payload and re-runs
the encoding-selection logic, so a wrong listpack/intset/quicklist/hashtable
decision there is invisible to a byte-compat check yet visible to clients and
monitoring (cf. the encoding-after-reload class: hpfey). This gate builds values
spanning every type and both sides of the small/large encoding boundary, DUMPs +
RESTOREs each on each server, and asserts:
  (a) the original key's OBJECT ENCODING matches between fr and redis, and
  (b) the RESTORE'd key's OBJECT ENCODING matches between fr and redis.

Both servers run config-LESS (compiled defaults) so encodings align without the
config-default false-positive class.

SETUP:
  ORACLE=legacy_redis_code/redis/src
  $ORACLE/redis-server --port 17831 --daemonize yes --save '' --appendonly no
  $CARGO_TARGET_DIR/debug/frankenredis --port 17832 --mode strict &
  scripts/restore_encoding_differ.py --oracle 17831 --fr 17832

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
            a = a if isinstance(a, bytes) else str(a).encode()
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
            return d
        if t == b"*":
            n = int(rest)
            if n < 0:
                return line
            return line + b"|" + b"|".join(self._read() for _ in range(n))
        return line


def build(c):
    """Populate one value per (type x encoding-side-of-boundary)."""
    c.cmd("FLUSHALL")
    c.cmd("RPUSH", "Lsmall", "a", "b", "c")
    c.cmd("RPUSH", "Lbig", *[f"elem{i:05}" for i in range(200)])
    c.cmd("RPUSH", "Lbigval", "x" * 128)  # value-length forces quicklist
    c.cmd("SADD", "Sint", "1", "2", "3")  # intset
    c.cmd("SADD", "Sintbig", *[str(i) for i in range(600)])  # intset -> hashtable
    c.cmd("SADD", "Slp", "a", "b", "c")  # listpack
    c.cmd("SADD", "Sbig", *[f"m{i}" for i in range(200)])  # listpack -> hashtable
    c.cmd("HSET", "Hsmall", "a", "1", "b", "2")
    for i in range(200):
        c.cmd("HSET", "Hbig", f"f{i}", f"v{i}")
    c.cmd("ZADD", "Zsmall", "1", "a", "2", "b")
    for i in range(200):
        c.cmd("ZADD", "Zbig", i, f"m{i}")
    c.cmd("SET", "Sint64", "12345")  # int encoding
    c.cmd("SET", "Sembstr", "hello")  # embstr
    c.cmd("SET", "Sraw", "x" * 64)  # raw


KEYS = ["Lsmall", "Lbig", "Lbigval", "Sint", "Sintbig", "Slp", "Sbig",
        "Hsmall", "Hbig", "Zsmall", "Zbig", "Sint64", "Sembstr", "Sraw"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    o = Conn(args.oracle)
    f = Conn(args.fr)
    build(o)
    build(f)

    diffs = 0
    for k in KEYS:
        # Original-encoding parity.
        oo, of = o.cmd("OBJECT", "ENCODING", k), f.cmd("OBJECT", "ENCODING", k)
        if oo != of:
            diffs += 1
            print(f"ORIG-ENC DIVERGE {k}: oracle={oo!r} fr={of!r}")
        # DUMP + RESTORE on each server, then compare the RESTORE'd encoding.
        for c in (o, f):
            payload = c.cmd("DUMP", k)
            c.cmd("DEL", k + "_r")
            resp = c.cmd("RESTORE", k + "_r", "0", payload)
            if not resp.startswith(b"+OK"):
                print(f"RESTORE failed for {k}: {resp!r}")
        eo, ef = o.cmd("OBJECT", "ENCODING", k + "_r"), f.cmd("OBJECT", "ENCODING", k + "_r")
        if eo != ef:
            diffs += 1
            print(f"RESTORE-ENC DIVERGE {k}: oracle={eo!r} fr={ef!r}")

    if diffs:
        print(f"\nFAIL: {diffs} encoding divergence(s) (original and/or post-RESTORE)")
        sys.exit(1)
    print("OK: OBJECT ENCODING byte-exact for original AND post-RESTORE values "
          "vs redis 7.2.4 (list/set/intset/hash/zset/string across encoding boundaries)")


if __name__ == "__main__":
    main()
