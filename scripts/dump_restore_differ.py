#!/usr/bin/env python3
"""Self-launching cross-server DUMP/RESTORE interop gate vs redis 7.2.4.

The DUMP wire format (RDB-serialized value + 2-byte version + 8-byte CRC64) must
be byte-compatible BOTH WAYS between frankenredis and redis 7.2.4:

  * redis DUMP <k>  ->  fr RESTORE      (fr must decode redis's serialization)
  * fr    DUMP <k>  ->  redis RESTORE   (redis must decode fr's serialization)

This is the direction `rdb_cross_load_probe.sh` does NOT cover — that probe only
loads redis's static RDB assets into fr; here we prove real redis 7.2.4 can load
fr's own DUMP output for every object encoding, and that the round-tripped value
is semantically identical (read back and compared, not just RESTORE==OK).

Covers all encodings: string int/embstr/raw, list listpack+quicklist, set
intset/listpack/hashtable, hash listpack/hashtable, zset listpack/skiplist,
stream. fr-persist (DUMP/RESTORE serialization) has carried real byte-fidelity
bugs (lzf wmh2p, collection-element encoding bfg6z) — this hard-gates them.
"""
import argparse
import os
import socket
import subprocess
import sys
import time

REDIS_PORT = 21860
FR_PORT = 21861


def find_bin(explicit):
    if explicit:
        return explicit
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    for c in ("/data/tmp/cargo-target/release/frankenredis",
              "/data/tmp/cargo-target/debug/frankenredis",
              os.path.join(root, "target/release/frankenredis"),
              os.path.join(root, "target/debug/frankenredis")):
        if os.path.exists(c):
            return c
    return None


def find_redis(explicit):
    if explicit:
        return explicit
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    for c in (os.path.join(root, "legacy_redis_code/redis/src/redis-server"),
              os.path.join(root, "legacy_redis_code/src/redis-server")):
        if os.path.exists(c):
            return c
    return None


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(5.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            chunk = self.s.recv(65536)
            if not chunk:
                raise OSError("closed")
            self.b += chunk
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def _rn(self, n):
        while len(self.b) < n + 2:
            self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n)
        if t == b":":
            return int(r)
        if t == b"+":
            return r
        if t == b"-":
            return b"ERR:" + r
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


def launch(cmdline, port):
    proc = subprocess.Popen(cmdline, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, start_new_session=True)
    for _ in range(80):
        try:
            c = Conn(port)
            if c.cmd("PING") == b"PONG":
                return proc, c
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"server on port {port} did not start: {cmdline[0]}")


def seed(c):
    """Populate one server with a key per target encoding; return key list."""
    c.cmd("FLUSHALL")
    # strings
    c.cmd("SET", "s_int", "12345")
    c.cmd("SET", "s_embstr", "a short string")
    c.cmd("SET", "s_raw", "x" * 200)
    # lists: listpack (small) and quicklist (large / long elems)
    c.cmd("RPUSH", "l_lp", *[str(i) for i in range(10)])
    c.cmd("RPUSH", "l_ql", *[str(i) for i in range(400)])
    c.cmd("RPUSH", "l_ql_big", "y" * 200, "z" * 200)
    # sets: intset, listpack (small str), hashtable (large)
    c.cmd("SADD", "set_intset", *[str(i * 3) for i in range(20)])
    c.cmd("SADD", "set_lp", "alpha", "beta", "gamma")
    c.cmd("SADD", "set_ht", *["m%d" % i for i in range(300)])
    # hashes: listpack and hashtable
    c.cmd("HSET", "h_lp", "f1", "v1", "f2", "v2")
    c.cmd("HSET", "h_ht", *sum([["f%d" % i, "v%d" % i] for i in range(300)], []))
    # zsets: listpack and skiplist
    c.cmd("ZADD", "z_lp", "1", "a", "2.5", "b", "3", "c")
    c.cmd("ZADD", "z_sl", *sum([[str(i * 1.5), "z%d" % i] for i in range(300)], []))
    # stream
    for i in range(40):
        c.cmd("XADD", "stream", "*", "field", str(i))
    c.cmd("XGROUP", "CREATE", "stream", "g1", "0")
    return [
        "s_int", "s_embstr", "s_raw",
        "l_lp", "l_ql", "l_ql_big",
        "set_intset", "set_lp", "set_ht",
        "h_lp", "h_ht",
        "z_lp", "z_sl",
        "stream",
    ]


def readback(c, key):
    """Canonical, order-stable representation of a key's value for comparison."""
    t = c.cmd("TYPE", key)
    if t == b"string":
        return ("string", c.cmd("GET", key))
    if t == b"list":
        return ("list", c.cmd("LRANGE", key, "0", "-1"))
    if t == b"set":
        members = c.cmd("SMEMBERS", key) or []
        return ("set", sorted(members))
    if t == b"hash":
        flat = c.cmd("HGETALL", key) or []
        pairs = sorted((flat[i], flat[i + 1]) for i in range(0, len(flat), 2))
        return ("hash", pairs)
    if t == b"zset":
        return ("zset", c.cmd("ZRANGE", key, "0", "-1", "WITHSCORES"))
    if t == b"stream":
        return ("stream", c.cmd("XRANGE", key, "-", "+"),
                c.cmd("XLEN", key))
    return ("other", c.cmd("DUMP", key))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None)
    ap.add_argument("--redis-bin", default=None)
    args = ap.parse_args()
    binpath = find_bin(args.bin)
    redispath = find_redis(args.redis_bin)
    if not binpath or not os.path.exists(binpath):
        print("FAIL: frankenredis binary not found (pass --bin PATH)", file=sys.stderr)
        sys.exit(2)
    if not redispath or not os.path.exists(redispath):
        print("FAIL: redis-server not found (pass --redis-bin PATH)", file=sys.stderr)
        sys.exit(2)

    failures = []
    procs = []
    try:
        p, rc = launch([redispath, "--port", str(REDIS_PORT), "--save", "",
                        "--appendonly", "no"], REDIS_PORT)
        procs.append(p)
        p, fc = launch([binpath, "--port", str(FR_PORT)], FR_PORT)
        procs.append(p)

        keys = seed(rc)
        seed(fc)

        for k in keys:
            # Direction 1: redis DUMP -> fr RESTORE, fr value must equal redis's.
            blob = rc.cmd("DUMP", k)
            if not isinstance(blob, (bytes, bytearray)):
                failures.append(f"{k}: redis DUMP returned {blob!r}")
                continue
            res = fc.cmd("RESTORE", "r2f_" + k, "0", blob, "REPLACE")
            if res != b"OK":
                failures.append(f"{k}: redis->fr RESTORE failed: {res!r}")
            else:
                want = readback(rc, k)
                got = readback(fc, "r2f_" + k)
                if want != got:
                    failures.append(f"{k}: redis->fr value mismatch\n      redis={want}\n      fr   ={got}")

            # Direction 2: fr DUMP -> redis RESTORE, redis value must equal fr's.
            blob = fc.cmd("DUMP", k)
            if not isinstance(blob, (bytes, bytearray)):
                failures.append(f"{k}: fr DUMP returned {blob!r}")
                continue
            res = rc.cmd("RESTORE", "f2r_" + k, "0", blob, "REPLACE")
            if res != b"OK":
                failures.append(f"{k}: fr->redis RESTORE failed: {res!r}")
            else:
                want = readback(fc, k)
                got = readback(rc, "f2r_" + k)
                if want != got:
                    failures.append(f"{k}: fr->redis value mismatch\n      fr   ={want}\n      redis={got}")
    finally:
        for p in reversed(procs):
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()

    if failures:
        print("FAIL: DUMP/RESTORE cross-server interop divergences:")
        for fl in failures:
            print(f"  - {fl}")
        sys.exit(1)
    print(f"OK: DUMP/RESTORE byte-compatible both ways vs redis 7.2.4 "
          f"({len(keys)} keys across all encodings; value round-trip verified)")
    sys.exit(0)


if __name__ == "__main__":
    main()
