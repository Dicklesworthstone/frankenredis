#!/usr/bin/env python3
"""index_range_boundary_gate.py — index/range/count boundary parity vs redis 7.2.4.

Bugs in the deterministic range commands cluster at the *argument boundaries*:
negative, zero, and out-of-range index / rank / count / limit values, plus the
BYTE-vs-BIT and REV / BYSCORE / BYLEX modifier combinations. The curated edge
sweeps exercise option combos and error wording; this gate instead sweeps a dense
boundary *matrix* (~680 cases) over a fixed keyspace and asserts the reply bytes
match the vendored oracle exactly.

Commands covered: LRANGE, LINDEX, GETRANGE, LPOS (RANK/COUNT), SINTERCARD (LIMIT),
ZRANGE (index / BYSCORE / BYLEX, REV, LIMIT), ZRANGEBYSCORE, ZRANGEBYLEX,
BITCOUNT (BYTE/BIT), BITPOS (BYTE/BIT), ZRANK / ZREVRANK (WITHSCORE).

Random-selection commands are deliberately excluded (non-deterministic replies).
Self-launches both servers (compiled defaults). Exit 0 parity / 1 divergence /
2 harness failure.
"""
import argparse
import itertools
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time


def reply(s):
    data = bytearray()

    def line():
        l = bytearray()
        while not l.endswith(b"\r\n"):
            ch = s.recv(1)
            if not ch:
                break
            l += ch
        return bytes(l)

    def one():
        l = line()
        data.extend(l)
        if not l:
            return
        t = l[:1]
        if t in (b"$", b"="):
            n = int(l[1:])
            if n >= 0:
                need = n + 2
                while need > 0:
                    c = s.recv(need)
                    data.extend(c)
                    need -= len(c)
        elif t in (b"*", b"~", b">", b"%"):
            n = int(l[1:])
            mult = 2 if t == b"%" else 1
            for _ in range(max(n, 0) * mult):
                one()

    one()
    return bytes(data)


class Conn:
    def __init__(self, port, timeout=8):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.s.settimeout(timeout)

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            a = a.encode() if isinstance(a, str) else a
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return reply(self.s)


def setup(c):
    c.cmd("FLUSHALL")
    c.cmd("RPUSH", "L", *[str(i) for i in range(10)])
    c.cmd("SET", "STR", "Hello World")
    c.cmd("SADD", "S1", "a", "b", "c", "d", "e")
    c.cmd("SADD", "S2", "c", "d", "e", "f", "g")
    c.cmd("ZADD", "Z", *list(itertools.chain(*[(str(i), "m%d" % i) for i in range(10)])))
    c.cmd("HSET", "H", *list(itertools.chain(*[("f%d" % i, str(i)) for i in range(6)])))
    c.cmd("SETBIT", "BM", "100", "1")
    c.cmd("SET", "BS", "foobar")


IDX = ["-100", "-11", "-10", "-1", "0", "1", "9", "10", "11", "100"]


def build_cases():
    cases = []
    for a in IDX:
        for b in IDX:
            cases.append(("LRANGE", "L", a, b))
            cases.append(("GETRANGE", "STR", a, b))
            cases.append(("ZRANGE", "Z", a, b))
            cases.append(("ZRANGE", "Z", a, b, "REV"))
    for a in IDX:
        cases.append(("LINDEX", "L", a))
    for rank in ["-3", "-1", "1", "3"]:
        for count in ["0", "1", "2"]:
            cases.append(("LPOS", "L", "5", "RANK", rank, "COUNT", count))
    cases.append(("LPOS", "L", "5"))
    cases.append(("LPOS", "L", "999"))
    for lim in ["0", "1", "2", "100"]:
        cases.append(("SINTERCARD", "2", "S1", "S2", "LIMIT", lim))
    for lo, hi in [("-inf", "+inf"), ("(1", "5"), ("3", "3"), ("5", "1")]:
        cases.append(("ZRANGE", "Z", lo, hi, "BYSCORE"))
        cases.append(("ZRANGE", "Z", lo, hi, "BYSCORE", "LIMIT", "1", "2"))
        cases.append(("ZRANGEBYSCORE", "Z", lo, hi, "LIMIT", "0", "-1"))
    for lo, hi in [("-", "+"), ("[m1", "[m5"), ("(m1", "(m5"), ("+", "-")]:
        cases.append(("ZRANGE", "Z", lo, hi, "BYLEX"))
        cases.append(("ZRANGEBYLEX", "Z", lo, hi, "LIMIT", "1", "-1"))
    for a in ["-100", "-6", "-1", "0", "1", "5", "100"]:
        for b in ["-100", "-1", "0", "1", "5", "100"]:
            cases.append(("BITCOUNT", "BS", a, b))
            cases.append(("BITCOUNT", "BS", a, b, "BYTE"))
            cases.append(("BITCOUNT", "BS", a, b, "BIT"))
            cases.append(("BITPOS", "BS", "1", a, b, "BIT"))
            cases.append(("BITPOS", "BS", "0", a, b, "BYTE"))
    for m in ["m0", "m5", "m9", "nope"]:
        cases.append(("ZRANK", "Z", m))
        cases.append(("ZRANK", "Z", m, "WITHSCORE"))
        cases.append(("ZREVRANK", "Z", m, "WITHSCORE"))
    return cases


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") in (b"+PONG\r\n",):
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

    rdir = tempfile.mkdtemp(prefix="fr_idxrange_")
    fp, rp = free_port(), free_port()
    procs = []
    try:
        procs.append(subprocess.Popen(
            [fr, "--port", str(fp), "--rdb", os.path.join(rdir, "fr.rdb")],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen(
            [redis, "--port", str(rp), "--dir", rdir, "--save", "", "--appendonly", "no"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start")
            return 2

        fc, rc = Conn(fp), Conn(rp)
        setup(rc)
        setup(fc)
        cases = build_cases()
        diffs = []
        for c in cases:
            ro, rf = rc.cmd(*c), fc.cmd(*c)
            if ro != rf:
                diffs.append((c, ro, rf))
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=5)
            except Exception:
                p.kill()
        shutil.rmtree(rdir, ignore_errors=True)

    if diffs:
        print(f"FAIL: {len(diffs)} index/range boundary divergence(s) vs redis 7.2.4:")
        for c, ro, rf in diffs[:40]:
            print(f"  {c}\n    redis={ro!r}\n    fr   ={rf!r}")
        return 1
    print(f"OK: {len(cases)} index/range/count boundary cases byte-exact vs redis 7.2.4 "
          "(LRANGE/GETRANGE/LPOS/SINTERCARD/ZRANGE*/BITCOUNT/BITPOS/ZRANK)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
