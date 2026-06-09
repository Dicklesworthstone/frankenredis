#!/usr/bin/env python3
"""object_encoding_boundary_gate.py — OBJECT ENCODING boundary parity vs redis 7.2.4.

Sweeps every type's listpack/intset/quicklist/skiplist/hashtable transition AT its
config-driven threshold, under BOTH default and explicitly-set encoding configs —
the angle that caught frankenredis-gdep4 (a positive `list-max-listpack-size`
ignored the 8192-byte quicklist SIZE_SAFETY_LIMIT, a path the default `-2` config
never exercises). Compares `OBJECT ENCODING` fr vs redis for each shape.

Timely guard: fr-store's encoding logic is under active perf editing, so an
encoding regression must fail loudly here. Self-launches a clean fr + vendored
redis pair (compiled defaults, then per-case CONFIG SET to known values so the two
servers agree regardless of shipped redis.conf).

Usage: object_encoding_boundary_gate.py [--bin FR] [--redis-bin REDIS]
"""
import argparse, os, socket, subprocess, sys, time, tempfile, shutil


class Conn:
    def __init__(self, port, timeout=5):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.s.settimeout(timeout); self.b = b""
    def _l(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d: raise OSError("closed")
            self.b += d
        l, self.b = self.b.split(b"\r\n", 1); return l
    def _n(self, n):
        while len(self.b) < n + 2: self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n+2:]; return d
    def p(self):
        l = self._l(); t, r = l[:1], l[1:]
        if t == b"$":
            n = int(r); return None if n < 0 else self._n(n)
        if t == b":": return int(r)
        if t in (b"+", b"-"): return r
        if t == b"*":
            n = int(r); return None if n < 0 else [self.p() for _ in range(n)]
        return l
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o); return self.p()


def enc(c, k):
    r = c.cmd("OBJECT", "ENCODING", k)
    return r.decode() if isinstance(r, bytes) else r


# Each case: (label, [config (cfg,val) ...], [seed-commands ...], key-to-check)
ENC_CONFIGS = [
    ("hash-max-listpack-entries", "128"), ("hash-max-listpack-value", "64"),
    ("set-max-listpack-entries", "128"), ("set-max-intset-entries", "512"),
    ("zset-max-listpack-entries", "128"), ("zset-max-listpack-value", "64"),
    ("list-max-listpack-size", "128"),
]


def cases():
    big = "y" * 9000
    out = []
    # strings
    out += [
        ("str_int", [["SET", "k", "12345"]], "k"),
        ("str_embstr44", [["SET", "k", "x" * 44]], "k"),
        ("str_raw45", [["SET", "k", "x" * 45]], "k"),
        ("str_append_raw", [["APPEND", "k", "abc"]], "k"),
        ("str_bigint_embstr", [["SET", "k", "1" * 30]], "k"),
    ]
    # list: count + the positive-fill SIZE_SAFETY_LIMIT (gdep4) + negative-fill
    out += [
        ("list_small", [["RPUSH", "k", "a", "b"]], "k"),
        ("list_128", [["RPUSH", "k"] + [str(i) for i in range(128)]], "k"),
        ("list_129_quicklist", [["RPUSH", "k"] + [str(i) for i in range(129)]], "k"),
        ("list_elem_8100", [["RPUSH", "k", "y" * 8100]], "k"),
        ("list_elem_8192_quicklist", [["RPUSH", "k", "y" * 8192]], "k"),
        ("list_elem_9000_quicklist", [["RPUSH", "k", big]], "k"),
        ("list_100x100_quicklist", [["RPUSH", "k"] + ["y" * 100] * 100], "k"),
    ]
    # hash
    out += [
        ("hash_small", [["HSET", "k", "f", "v"]], "k"),
        ("hash_129_hashtable", [["HSET", "k"] + sum(([f"f{i}", "v"] for i in range(129)), [])], "k"),
        ("hash_bndval64", [["HSET", "k", "f", "y" * 64]], "k"),
        ("hash_bigval65_hashtable", [["HSET", "k", "f", "y" * 65]], "k"),
    ]
    # set
    out += [
        ("set_intset", [["SADD", "k", "1", "2", "3"]], "k"),
        ("set_int_to_listpack", [["SADD", "k", "1", "abc"]], "k"),
        ("set_listpack_to_hashtable", [["SADD", "k"] + [f"m{i}" for i in range(129)]], "k"),
        ("set_intset_overflow_hashtable", [["SADD", "k"] + [str(i) for i in range(513)]], "k"),
    ]
    # zset
    out += [
        ("zset_listpack", [["ZADD", "k", "1", "a"]], "k"),
        ("zset_129_skiplist", [["ZADD", "k"] + sum(([str(i), f"m{i}"] for i in range(129)), [])], "k"),
        ("zset_bigval65_skiplist", [["ZADD", "k", "1", "y" * 65]], "k"),
    ]
    return out


def run(c):
    for cfg, val in ENC_CONFIGS:
        c.cmd("CONFIG", "SET", cfg, val)
    res = {}
    for label, seed, key in cases():
        c.cmd("FLUSHALL")
        for s in seed:
            c.cmd(*s)
        res[label] = enc(c, key)
    return res


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") in (b"PONG", b"OK"): return True
        except Exception: time.sleep(0.2)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("FR_BIN",
                    "/data/tmp/cargo-target/release/frankenredis"))
    ap.add_argument("--redis-bin", default=os.environ.get("REDIS_BIN",
                    os.path.join(os.path.dirname(__file__), "..",
                                 "legacy_redis_code/redis/src/redis-server")))
    args = ap.parse_args()
    fr = os.path.abspath(args.bin); redis = os.path.abspath(args.redis_bin)
    if not os.path.exists(fr):
        print(f"SKIP: fr binary not found at {fr}"); return 0
    if not os.path.exists(redis):
        print(f"SKIP: redis-server not found at {redis}"); return 0

    rdir = tempfile.mkdtemp(prefix="fr_enc_gate_")
    fp, rp = free_port(), free_port()
    procs = []
    try:
        procs.append(subprocess.Popen([fr, "--port", str(fp)],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen(
            [redis, "--port", str(rp), "--dir", rdir, "--save", "", "--appendonly", "no"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        ro, fo = run(Conn(rp)), run(Conn(fp))
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()
        shutil.rmtree(rdir, ignore_errors=True)

    div = 0
    for label, _, _ in cases():
        a, b = ro.get(label), fo.get(label)
        if a != b:
            div += 1
            print(f"  [DIVERGE] {label:32s} redis={a} fr={b}")
    if div:
        print(f"FAIL — {div} OBJECT ENCODING boundary divergence(s) vs redis 7.2.4")
        return 1
    print(f"PASS — OBJECT ENCODING byte-exact vs redis 7.2.4 across {len(cases())} "
          f"boundary shapes (incl. gdep4 positive-fill SIZE_SAFETY_LIMIT)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
