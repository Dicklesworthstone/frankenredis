#!/usr/bin/env python3
"""replication_multi_wrap_gate.py — MULTI/EXEC propagation-wrapping fidelity gate.

Vendored Redis wraps a unit of work's propagated effects in MULTI ... EXEC in
the replication / AOF stream ONLY when MORE THAN ONE command propagates, so the
replica and AOF replay the effects atomically. Exactly one effect propagates
bare; zero effects propagate nothing. This gate pins that fr matches the upstream
`propagatePendingCommands` threshold across the unit kinds that can emit several
effects, plus the cases that must NOT be wrapped:

  - MULTI/EXEC transaction: 0 writes -> nothing; 1 write -> bare; >=2 -> wrapped.
  - Lua script (EVAL): 1 redis.call write -> bare; >=2 -> wrapped.
  - Key expiry (lazy on access, and a write that recreates a just-expired key):
    the expiry DEL/UNLINK propagates as a BARE standalone command, never wrapped
    with the triggering command — upstream propagates each expiration as its own
    execution unit (verified differentially against redis 7.2.4: a lazy-expire
    DEL + the recreating SET are two bare commands on both, NOT a MULTI/EXEC).

This is an fr-SELF invariant gate that reads the on-disk AOF (the same byte
stream fed to replicas), which is deterministic and reliable — unlike a
replica-side MONITOR capture, whose timing is racy. The behavior asserted here is
well-defined regardless of any running oracle.

Usage: replication_multi_wrap_gate.py [--bin PATH]
Exit 0 if every wrapping invariant holds, else 1.
"""
import argparse
import os
import re
import socket
import subprocess
import sys
import tempfile
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(3.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
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
        if t == b"+":
            return r.decode("latin1")
        if t == b":":
            return int(r)
        if t == b"-":
            return "ERR:" + r.decode("latin1")
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
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


def find_bin():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    cands = [
        "/data/tmp/cargo-target/release/frankenredis",
        "/data/tmp/cargo-target/debug/frankenredis",
        os.path.join(root, "target/release/frankenredis"),
        os.path.join(root, "target/debug/frankenredis"),
    ]
    for c in cands:
        if os.path.exists(c):
            return c
    return None


def aof_commands(aof_dir):
    """Decode the incr AOF into a list of commands (each a list of arg strings)."""
    incr = None
    for f in os.listdir(aof_dir):
        if f.endswith(".incr.aof"):
            incr = os.path.join(aof_dir, f)
    if incr is None:
        return []
    data = open(incr, "rb").read()
    cmds = []
    i = 0
    while i < len(data):
        if data[i:i + 1] != b"*":
            break
        j = data.index(b"\r\n", i)
        nargs = int(data[i + 1:j])
        i = j + 2
        args = []
        for _ in range(nargs):
            assert data[i:i + 1] == b"$"
            j = data.index(b"\r\n", i)
            ln = int(data[i + 1:j])
            i = j + 2
            args.append(data[i:i + ln].decode("latin1"))
            i += ln + 2
        cmds.append(args)
    return cmds


def verbs_between(cmds, beg_key, end_key):
    """Uppercased verbs of commands strictly between the SET <beg_key> and
    SET <end_key> markers, dropping SELECT bookkeeping."""
    out = []
    on = False
    for c in cmds:
        if not c:
            continue
        v = c[0].upper()
        if v == "SET" and len(c) > 1 and c[1] == beg_key:
            on = True
            continue
        if v == "SET" and len(c) > 1 and c[1] == end_key:
            break
        if not on or v == "SELECT":
            continue
        out.append(v)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None)
    args = ap.parse_args()
    binpath = args.bin or find_bin()
    if not binpath or not os.path.exists(binpath):
        print("FAIL: frankenredis binary not found (pass --bin PATH)", file=sys.stderr)
        sys.exit(2)

    tmp = tempfile.mkdtemp(prefix="fr-wrapgate-")
    port = 21717
    proc = subprocess.Popen(
        [binpath, "--port", str(port), "--aof", os.path.join(tmp, "appendonly.aof")],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    failures = []
    try:
        for _ in range(50):
            try:
                c = Conn(port)
                if c.cmd("PING") == "PONG":
                    break
            except OSError:
                time.sleep(0.1)
        else:
            print("FAIL: server did not start", file=sys.stderr)
            sys.exit(2)

        c.cmd("FLUSHALL")

        # 0-write transaction -> nothing.
        c.cmd("SET", "__t0b__", "1")
        c.cmd("MULTI"); c.cmd("GET", "x"); c.cmd("EXEC")
        c.cmd("SET", "__t0e__", "1")
        # 1-write transaction -> bare.
        c.cmd("MULTI"); c.cmd("SET", "ta", "1"); c.cmd("EXEC")
        c.cmd("SET", "__t1e__", "1")
        # 2-write transaction -> wrapped.
        c.cmd("MULTI"); c.cmd("SET", "tb", "1"); c.cmd("SET", "tc", "2"); c.cmd("EXEC")
        c.cmd("SET", "__t2e__", "1")
        # 1-write script -> bare.
        c.cmd("EVAL", "redis.call('set','sa','1'); return 1", "0")
        c.cmd("SET", "__s1e__", "1")
        # 2-write script -> wrapped.
        c.cmd("EVAL", "redis.call('set','sb','1'); redis.call('set','sc','2'); return 1", "0")
        c.cmd("SET", "__s2e__", "1")
        # Lazy-expire DEL + recreate -> two BARE commands (no wrap).
        c.cmd("SET", "ek", "v", "PX", "30")
        time.sleep(0.1)
        c.cmd("SET", "ek", "new")
        c.cmd("SET", "__exe__", "1")

        time.sleep(0.3)
        cmds = aof_commands(tmp)

        def check(name, beg, end, expected):
            got = verbs_between(cmds, beg, end)
            if got != expected:
                failures.append(f"{name}: expected {expected}, got {got}")

        check("0-write txn (nothing)", "__t0b__", "__t0e__", [])
        check("1-write txn (bare)", "__t0e__", "__t1e__", ["SET"])
        check("2-write txn (wrapped)", "__t1e__", "__t2e__",
              ["MULTI", "SET", "SET", "EXEC"])
        check("1-write script (bare)", "__t2e__", "__s1e__", ["SET"])
        check("2-write script (wrapped)", "__s1e__", "__s2e__",
              ["MULTI", "SET", "SET", "EXEC"])
        # lazy-expire DEL + recreate: the expiry deletion and the recreating SET
        # propagate as BARE standalone commands — never wrapped in MULTI/EXEC.
        # (Window also contains the initial `SET ek v PX..`; what matters is that
        # a DEL/UNLINK is propagated and that NO MULTI/EXEC frame appears.)
        exp = verbs_between(cmds, "__s2e__", "__exe__")
        if "MULTI" in exp or "EXEC" in exp:
            failures.append(
                f"lazy-expire recreate must NOT be MULTI/EXEC-wrapped, got {exp}")
        if "DEL" not in exp and "UNLINK" not in exp:
            failures.append(
                f"lazy-expire deletion was not propagated, got {exp}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()

    if failures:
        print("FAIL: MULTI/EXEC propagation-wrapping divergences:")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    print("OK: MULTI/EXEC propagation wrapping matches the upstream "
          ">1-effect threshold (txn, script, and bare-expiry invariants hold)")


if __name__ == "__main__":
    main()
