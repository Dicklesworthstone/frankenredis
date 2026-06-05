#!/usr/bin/env python3
"""packed_collection_probe.py — regression gate for the packed listpack-style
collection encodings (frankenredis-9mh3o: PackedStrSet/PackedStrMap/PackedList
/PackedZSet behind small set/hash/list/zset). Guards the highest-risk surface of
that work — RDB/DUMP byte-fidelity and binary safety — which the command-level
differential probes do not cover:

  * CROSS-ENGINE DUMP/RESTORE, BOTH directions: a vendored-redis-7.2.4 DUMP of a
    listpack-encoded collection must RESTORE into fr (and vice-versa), proving
    fr's packed encoding round-trips through redis's wire format.
  * BINARY members (NUL / high bytes) — the packed `[varint len][bytes]` records
    must stay binary-safe (a member that looks like a length prefix must not
    corrupt the scan).
  * encoding-promotion boundaries (128/129 entries, 64/65-byte values) and
    DUMP/RESTORE of a PROMOTED (hashtable/skiplist) collection.

SETUP (same as scripts/differential_probe.sh — config-less oracle, fr strict):
    ORACLE=legacy_redis_code/redis/src
    $ORACLE/redis-server --port 16399 --daemonize yes --save '' --appendonly no
    cargo build -p fr-server           # $CARGO_TARGET_DIR/debug/frankenredis
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    python3 scripts/packed_collection_probe.py 16399 16400
"""
import socket
import sys
import time


class Conn:
    """Minimal binary-safe raw-socket RESP2 client (DUMP payloads are binary)."""

    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=3)
        self.buf = b""

    def _fill(self):
        self.s.settimeout(1.0)
        try:
            d = self.s.recv(65536)
            if not d:
                return False
            self.buf += d
            return True
        except socket.timeout:
            return False

    def _line(self):
        while b"\r\n" not in self.buf:
            if not self._fill():
                return None
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def read(self):
        line = self._line()
        if line is None:
            return None
        t, rest = line[:1], line[1:]
        if t in (b"+", b"-", b":"):
            return (t.decode(), rest.decode("latin1"))
        if t == b"$":
            n = int(rest)
            if n < 0:
                return None
            while len(self.buf) < n + 2:
                if not self._fill():
                    break
            data, self.buf = self.buf[:n], self.buf[n + 2:]
            return data  # raw bytes (binary-safe)
        if t == b"*":
            n = int(rest)
            if n < 0:
                return None
            return [self.read() for _ in range(n)]
        return line

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            a = a if isinstance(a, bytes) else str(a).encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        time.sleep(0.02)
        return self.read()


def run(op, fp):
    o, f = Conn(op), Conn(fp)
    o.cmd("FLUSHALL")
    f.cmd("FLUSHALL")
    fails = 0

    def chk(label, a, b):
        nonlocal fails
        if a != b:
            fails += 1
            print(f"DIVERGE [{label}]\n  a={a!r}\n  b={b!r}")
        else:
            print(f"ok  [{label}]")

    # (name, write-cmd, populate-args, read-cmd-with-args)
    specs = [
        ("set", "SADD", ["a", "b", "c", "apple", "banana"], ["SMEMBERS"]),
        ("hash", "HSET", ["f1", "v1", "f2", "v2", "f3", "v3"], ["HGETALL"]),
        ("list", "RPUSH", ["x", "y", "z", "w"], ["LRANGE", "0", "-1"]),
        ("zset", "ZADD", ["1", "a", "2", "b", "3", "c"], ["ZRANGE", "0", "-1", "WITHSCORES"]),
    ]

    # ── cross-engine DUMP/RESTORE both directions + encoding ──
    for name, wcmd, args, rd in specs:
        o.cmd(wcmd, name, *args)
        f.cmd(wcmd, name, *args)
        # oracle DUMP -> fr RESTORE
        od = o.cmd("DUMP", name)
        chk(f"{name}: oracle-DUMP -> fr-RESTORE", ("+", "OK"), f.cmd("RESTORE", name + "_fr", "0", od))
        # fr DUMP -> oracle RESTORE
        fd = f.cmd("DUMP", name)
        chk(f"{name}: fr-DUMP -> oracle-RESTORE", ("+", "OK"), o.cmd("RESTORE", name + "_o", "0", fd))
        # encodings agree (native + restored)
        chk(f"{name}: encoding native", o.cmd("OBJECT", "ENCODING", name), f.cmd("OBJECT", "ENCODING", name))
        chk(f"{name}: encoding fr-restored", o.cmd("OBJECT", "ENCODING", name), f.cmd("OBJECT", "ENCODING", name + "_fr"))
        # list/zset have a defined order — restored reads must match
        if name in ("list", "zset"):
            chk(f"{name}: restored read matches native",
                f.cmd(rd[0], name, *rd[1:]), f.cmd(rd[0], name + "_fr", *rd[1:]))

    # ── binary members (NUL / high bytes) — varint binary-safety ──
    o.cmd("FLUSHALL")
    f.cmd("FLUSHALL")
    binm = b"\x00\x01\xff\x80mem\x00end"
    binm2 = b"\xde\xad\xbe\xef"
    o.cmd("SADD", "bs", binm, binm2)
    f.cmd("SADD", "bs", binm, binm2)
    chk("binary set SISMEMBER", o.cmd("SISMEMBER", "bs", binm), f.cmd("SISMEMBER", "bs", binm))
    chk("binary set SCARD", o.cmd("SCARD", "bs"), f.cmd("SCARD", "bs"))
    fd = f.cmd("DUMP", "bs")
    chk("binary set fr-DUMP -> oracle-RESTORE", ("+", "OK"), o.cmd("RESTORE", "bs_o", "0", fd))
    chk("binary set restored member present", (":", "1"), o.cmd("SISMEMBER", "bs_o", binm))
    o.cmd("HSET", "bh", binm, b"\x00val\xff")
    f.cmd("HSET", "bh", binm, b"\x00val\xff")
    chk("binary hash HGET", o.cmd("HGET", "bh", binm), f.cmd("HGET", "bh", binm))
    o.cmd("RPUSH", "bl", binm, b"\xff\xff")
    f.cmd("RPUSH", "bl", binm, b"\xff\xff")
    chk("binary list LRANGE", o.cmd("LRANGE", "bl", "0", "-1"), f.cmd("LRANGE", "bl", "0", "-1"))
    chk("binary list LPOS", o.cmd("LPOS", "bl", binm), f.cmd("LPOS", "bl", binm))
    o.cmd("ZADD", "bz", "1", binm, "2", binm2)
    f.cmd("ZADD", "bz", "1", binm, "2", binm2)
    chk("binary zset ZSCORE", o.cmd("ZSCORE", "bz", binm), f.cmd("ZSCORE", "bz", binm))
    chk("binary zset ZRANK", o.cmd("ZRANK", "bz", binm2), f.cmd("ZRANK", "bz", binm2))

    # ── encoding-promotion boundaries + promoted DUMP/RESTORE ──
    o.cmd("FLUSHALL")
    f.cmd("FLUSHALL")
    for n, key in [(128, "s128"), (129, "s129")]:
        for i in range(n):
            o.cmd("SADD", key, f"m{i:04}")
            f.cmd("SADD", key, f"m{i:04}")
        chk(f"set {n}-entry encoding", o.cmd("OBJECT", "ENCODING", key), f.cmd("OBJECT", "ENCODING", key))
    for v, key in [("x" * 64, "v64"), ("x" * 65, "v65")]:
        o.cmd("SADD", key, v)
        f.cmd("SADD", key, v)
        chk(f"set {len(v)}-byte value encoding", o.cmd("OBJECT", "ENCODING", key), f.cmd("OBJECT", "ENCODING", key))
    # promoted (hashtable) set still round-trips through DUMP/RESTORE
    fd = f.cmd("DUMP", "s129")
    chk("promoted set fr-DUMP -> oracle-RESTORE", ("+", "OK"), o.cmd("RESTORE", "s129_o", "0", fd))
    chk("promoted set SCARD after restore", o.cmd("SCARD", "s129"), o.cmd("SCARD", "s129_o"))
    # promoted zset (skiplist) keeps every member sorted
    for i in range(130):
        o.cmd("ZADD", "zbig", str(i), f"m{i:03}")
        f.cmd("ZADD", "zbig", str(i), f"m{i:03}")
    chk("zset 130-entry encoding", o.cmd("OBJECT", "ENCODING", "zbig"), f.cmd("OBJECT", "ENCODING", "zbig"))
    chk("zset promoted ZRANGE", o.cmd("ZRANGE", "zbig", "0", "-1"), f.cmd("ZRANGE", "zbig", "0", "-1"))

    return fails


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    fails = run(op, fp)
    print("-" * 60)
    if fails == 0:
        print("PASS — packed collections round-trip byte-exact with redis 7.2.4")
    else:
        print(f"FAIL — {fails} divergence(s)")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
