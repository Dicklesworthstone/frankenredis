#!/usr/bin/env python3
"""Admission probe for candidate balanced_square_ab shapes.

That harness's contract is explicit: a registered shape must give an identical
NON-ERROR reply on both engines, and the reply must be UNCHANGED after 200
repetitions. A shape that drifts (APPEND, INCR, LPUSH) silently measures a
different operation at op 1 and op 20000; a shape that errors on one engine
measures an error path, which redis-benchmark happily counts as a completed
request.

This probes candidates against BOTH engines before any of them is registered.
"""
import os
import re
import subprocess
import socket
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")

# (name, seed commands, probed command)
CANDIDATES = [
    ("strlen",        ["SET s abcdefghijklmnop"], ["STRLEN", "s"]),
    ("getrange",      ["SET s abcdefghijklmnop"], ["GETRANGE", "s", "2", "9"]),
    ("llen",          ["RPUSH l a b c d e"], ["LLEN", "l"]),
    ("lrange_5",      ["RPUSH l a b c d e"], ["LRANGE", "l", "0", "-1"]),
    ("hlen",          ["HSET h f1 v1 f2 v2 f3 v3"], ["HLEN", "h"]),
    ("hget",          ["HSET h f1 v1 f2 v2 f3 v3"], ["HGET", "h", "f2"]),
    ("scard",         ["SADD st m1 m2 m3 m4 m5"], ["SCARD", "st"]),
    ("zcard",         ["ZADD z 1 a 2 b 3 c"], ["ZCARD", "z"]),
    ("type",          ["SET s abcdefghijklmnop"], ["TYPE", "s"]),
    ("object_encoding", ["SET s abcdefghijklmnop"], ["OBJECT", "ENCODING", "s"]),
    ("ttl_nonvolatile", ["SET s abcdefghijklmnop"], ["TTL", "s"]),
    ("persist_noop",  ["SET s abcdefghijklmnop"], ["PERSIST", "s"]),
    # Writes whose reply is stable under repetition -- the write path is badly
    # under-represented in the existing sets, which are almost all reads.
    ("set_same",      [], ["SET", "wk", "vvvvvvvvvvvvvvvv"]),
    ("setex_same",    [], ["SETEX", "wx", "100", "vvvvvvvvvvvvvvvv"]),
    ("setrange_same", ["SET sr abcdefghijklmnop"], ["SETRANGE", "sr", "3", "xy"]),
    ("hset_same",     ["HSET h f1 v1"], ["HSET", "h", "f1", "v1"]),
    ("sadd_same",     ["SADD st m1"], ["SADD", "st", "m1"]),
    ("zadd_same",     ["ZADD z 1 a"], ["ZADD", "z", "1", "a"]),
    ("getex_persist", ["SET gx abcdefghijklmnop"], ["GETEX", "gx", "PERSIST"]),
    ("get_control",   ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
]

REPS = 200


def resp(*args) -> bytes:
    out = b"*%d\r\n" % len(args)
    for a in args:
        a = a if isinstance(a, bytes) else str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def free_port() -> int:
    probe = socket.socket()
    try:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]
    finally:
        probe.close()


class Client:
    """Minimal RESP client. Reads one whole reply by draining until the socket
    goes quiet, which is enough for the small fixed-size replies probed here."""

    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.s.settimeout(10)

    def call(self, *args):
        self.s.sendall(resp(*args))
        self.s.settimeout(2.0)
        buf = b""
        try:
            while True:
                chunk = self.s.recv(65536)
                if not chunk:
                    break
                buf += chunk
                if len(chunk) < 65536:
                    break
        except socket.timeout:
            pass
        self.s.settimeout(10)
        return buf

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def boot(binpath, extra):
    workdir = tempfile.mkdtemp(prefix="fr_admit_")
    port = free_port()
    proc = subprocess.Popen([os.path.abspath(binpath), "--port", str(port),
                             "--save", "", "--appendonly", "no"] + extra,
                            cwd=workdir, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
    client = None
    for _ in range(300):
        if proc.poll() is not None:
            raise SystemExit("%s exited during startup rc=%s" % (binpath, proc.returncode))
        try:
            client = Client(port)
            if b"PONG" in client.call("PING"):
                break
            client.close()
            client = None
        except OSError:
            time.sleep(0.1)
    if client is None:
        raise SystemExit("%s never became ready on %d" % (binpath, port))
    m = re.search(rb"process_id:(\d+)", client.call("INFO", "server"))
    assert m and int(m.group(1)) == proc.pid, "server on %d is not ours" % port
    return proc, client


def probe(client, seeds, cmd):
    client.call("FLUSHALL")
    for s in seeds:
        client.call(*s.split())
    first = client.call(*cmd)
    for _ in range(REPS - 1):
        last = client.call(*cmd)
    return first, last


def main():
    fr_bin = sys.argv[1]
    fr_proc, fr = boot(fr_bin, [])
    rd_proc, rd = boot(REDIS, [])
    admit, reject = [], []
    try:
        for name, seeds, cmd in CANDIDATES:
            f1, f2 = probe(fr, seeds, cmd)
            r1, r2 = probe(rd, seeds, cmd)
            why = []
            if f1.startswith(b"-") or r1.startswith(b"-"):
                why.append("ERROR reply")
            if f1 != f2 or r1 != r2:
                why.append("reply DRIFTS over %d reps" % REPS)
            if f1 != r1:
                why.append("engines DISAGREE (fr=%r redis=%r)" % (f1[:40], r1[:40]))
            if why:
                reject.append((name, "; ".join(why)))
            else:
                admit.append((name, f1[:32]))
        print("ADMIT (%d):" % len(admit))
        for n, r in admit:
            print("   %-18s reply=%r" % (n, r))
        print("\nREJECT (%d):" % len(reject))
        for n, w in reject:
            print("   %-18s %s" % (n, w))
    finally:
        for c, p in ((fr, fr_proc), (rd, rd_proc)):
            try:
                c.close()
            except Exception:
                pass
            p.terminate()
            try:
                p.wait(timeout=20)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait(timeout=10)


if __name__ == "__main__":
    main()
