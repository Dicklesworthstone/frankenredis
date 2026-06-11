#!/usr/bin/env python3
"""reload_encoding_survival_gate.py — OBJECT ENCODING must survive DEBUG RELOAD.

This is the regression guard for the RDB *load-path* encoding re-derivation bugs
fixed by frankenredis-hpfey (preserve_store_load_context carries the live
listpack/intset thresholds through reload), frankenredis-63p1s (apply live
encoding thresholds before the RDB rebuild) and frankenredis-39is8 (persist sets
by their actual encoding so a hashtable-encoded set survives the round-trip).

Distinct from the sibling gates:
  - config_persistence_reload_gate.py  checks CONFIG *values* survive reload.
  - encoding_config_boundary_differ.py checks OBJECT ENCODING at live CONFIG SET
    boundaries but never reloads.
This one is the only gate that asserts the *post-reload* OBJECT ENCODING of a
populated keyspace matches redis 7.2.4 byte-for-byte.

Method: lower every listpack/intset threshold so each collection genuinely spans
both sides of its encoding boundary, populate, snapshot OBJECT ENCODING, run
DEBUG RELOAD on both servers, snapshot again. fr fails if (a) it disagrees with
the oracle after reload, or (b) it silently flips its own encoding across reload
while the oracle keeps it stable.

Self-launches both servers (compiled defaults, --enable-debug-command yes).
Exit 0 on parity, 1 on divergence, 2 on harness/setup failure.
"""
import argparse
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time


class Conn:
    def __init__(self, port, timeout=8):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.s.settimeout(timeout)
        self.b = b""

    def _rd(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d:
                raise OSError("closed")
            self.b += d
        line, self.b = self.b.split(b"\r\n", 1)
        t = line[:1]
        if t in (b"+", b"-"):
            return line[1:]
        if t == b":":
            return int(line[1:])
        if t == b"$":
            n = int(line[1:])
            if n < 0:
                return None
            while len(self.b) < n + 2:
                self.b += self.s.recv(65536)
            d, self.b = self.b[:n], self.b[n + 2:]
            return d
        if t == b"*":
            n = int(line[1:])
            return None if n < 0 else [self._rd() for _ in range(n)]
        return line

    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o)
        return self._rd()

    def enc(self, key):
        r = self.cmd("OBJECT", "ENCODING", key)
        return r.decode() if isinstance(r, (bytes, bytearray)) else str(r)


# Thresholds lowered so every collection straddles its encoding boundary.
THRESHOLDS = [
    ("list-max-listpack-size", "8"),
    ("hash-max-listpack-entries", "8"),
    ("hash-max-listpack-value", "16"),
    ("set-max-listpack-entries", "8"),
    ("set-max-intset-entries", "8"),
    ("set-max-listpack-value", "16"),
    ("zset-max-listpack-entries", "8"),
    ("zset-max-listpack-value", "16"),
]


def populate(c):
    """Create keys spanning both sides of every encoding boundary. Returns key list."""
    c.cmd("FLUSHALL")
    for k, v in THRESHOLDS:
        c.cmd("CONFIG", "SET", k, v)
    keys = []

    def add(name, *cmd):
        c.cmd(*cmd)
        keys.append(name)

    # strings: int / embstr / raw
    add("s_int", "SET", "s_int", "12345")
    add("s_emb", "SET", "s_emb", "short")
    add("s_raw", "SET", "s_raw", "x" * 64)

    # lists: small (listpack) vs over-count (quicklist) vs over-value (quicklist)
    add("l_lp", "RPUSH", "l_lp", "a", "b", "c")
    add("l_ql_cnt", "RPUSH", "l_ql_cnt", *[str(i) for i in range(40)])
    add("l_ql_val", "RPUSH", "l_ql_val", "y" * 64)

    # hashes: small (listpack) vs over-count vs over-value (hashtable)
    add("h_lp", "HSET", "h_lp", "f1", "v1", "f2", "v2")
    big = []
    for i in range(40):
        big += ["f%d" % i, "v%d" % i]
    add("h_ht_cnt", "HSET", "h_ht_cnt", *big)
    add("h_ht_val", "HSET", "h_ht_val", "f", "w" * 64)

    # sets: intset vs listpack vs hashtable(over-count-int) vs hashtable(over-count-str)
    add("set_is", "SADD", "set_is", "1", "2", "3")
    add("set_lp", "SADD", "set_lp", "a", "bb", "ccc")
    add("set_ht_int", "SADD", "set_ht_int", *[str(i) for i in range(40)])
    add("set_ht_str", "SADD", "set_ht_str", *["m%d" % i for i in range(40)])
    add("set_ht_val", "SADD", "set_ht_val", "z" * 64)

    # zsets: small (listpack) vs over-count vs over-value (skiplist)
    add("z_lp", "ZADD", "z_lp", "1", "a", "2", "b")
    zbig = []
    for i in range(40):
        zbig += [str(i), "m%d" % i]
    add("z_sl_cnt", "ZADD", "z_sl_cnt", *zbig)
    add("z_sl_val", "ZADD", "z_sl_val", "1", "q" * 64)

    # stream (single encoding, but must survive the round-trip)
    c.cmd("XADD", "st", "1-1", "f", "v")
    add("st", "XADD", "st", "2-2", "f", "v")
    return keys


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") in (b"PONG", b"OK"):
                return True
        except Exception:
            time.sleep(0.2)
    return False


def find_fr():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    for c in (os.environ.get("FR_BIN", ""),
              "/data/tmp/cargo-target/release/frankenredis",
              "/data/tmp/cargo-target/debug/frankenredis",
              os.path.join(root, "target/release/frankenredis"),
              os.path.join(root, "target/debug/frankenredis")):
        if c and os.path.exists(c):
            return c
    return None


def find_redis():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    for c in (os.environ.get("REDIS_BIN", ""),
              os.path.join(root, "legacy_redis_code/redis/src/redis-server")):
        if c and os.path.exists(c):
            return c
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None)
    ap.add_argument("--redis-bin", default=None)
    args = ap.parse_args()
    fr = args.bin or find_fr()
    redis = args.redis_bin or find_redis()
    if not fr:
        print("SKIP: frankenredis binary not found (set FR_BIN or pass --bin)")
        return 0
    if not redis:
        print("SKIP: redis-server not found (set REDIS_BIN or pass --redis-bin)")
        return 0

    rdir = tempfile.mkdtemp(prefix="fr_reloadenc_")
    fp, rp = free_port(), free_port()
    procs = []
    try:
        procs.append(subprocess.Popen(
            [fr, "--port", str(fp), "--rdb", os.path.join(rdir, "fr.rdb"),
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen(
            [redis, "--port", str(rp), "--dir", rdir, "--save", "", "--appendonly", "no",
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start")
            return 2

        fc, rc = Conn(fp), Conn(rp)
        kf, kr = populate(fc), populate(rc)
        if kf != kr:
            print("FAIL: key sets diverged during populate")
            return 2

        pre = {k: (rc.enc(k), fc.enc(k)) for k in kr}
        rc.cmd("DEBUG", "RELOAD")
        fc.cmd("DEBUG", "RELOAD")
        post = {k: (rc.enc(k), fc.enc(k)) for k in kr}
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=5)
            except Exception:
                p.kill()
        shutil.rmtree(rdir, ignore_errors=True)

    diffs = []
    for k in kr:
        o_pre, f_pre = pre[k]
        o_post, f_post = post[k]
        if o_post != f_post:
            diffs.append(f"  [post-mismatch] {k}: redis={o_post!r} fr={f_post!r} "
                         f"(pre: redis={o_pre!r} fr={f_pre!r})")
        elif o_pre == o_post and f_pre != f_post:
            diffs.append(f"  [fr-self-flip] {k}: fr {f_pre!r}->{f_post!r} across reload; "
                         f"redis stable {o_pre!r}")

    if diffs:
        print("FAIL: OBJECT ENCODING diverged across DEBUG RELOAD:")
        print("\n".join(diffs))
        return 1
    print(f"OK: {len(kr)} keys — OBJECT ENCODING byte-exact across DEBUG RELOAD vs redis 7.2.4 "
          "(list/hash/set/zset/string boundaries + intset/value-length spans)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
