#!/usr/bin/env python3
"""expiry_spop_propagation_gate.py — propagation parity for the timer-driven /
non-deterministic write rewrites that aof_propagation_stream_gate EXPLICITLY
excludes (it only does fully-deterministic commands).

A master must propagate to AOF/replicas:
  - a key expiring LAZILY (on access)      -> DEL <key>      (NOT the read command)
  - a key expiring ACTIVELY (timer, no access) -> DEL <key>
  - SPOP <key> <count>                      -> SREM <key> <the members actually popped>
                                              (NOT verbatim SPOP, whose RNG would
                                               desync replicas)
Propagating any of these verbatim silently diverges a replica / corrupts the AOF
while local state still looks fine, so a behaviour differ can't catch it.

This launches fr and a config-less redis 7.2.4 with appendonly+appendfsync=always,
drives each scenario, parses the incr AOF, and checks the propagated FORM. The
expiry cases compare fr vs redis exactly (DEL <key>, deterministic). The SPOP
case compares the FORM (must be SREM) and asserts the propagated members equal
that server's own SPOP reply (each server pops its own random members, so the
member SET is self-consistent rather than cross-equal).

Usage: expiry_spop_propagation_gate.py [--bin FR] [--redis-bin REDIS]
Exit 0 if all propagation forms match redis 7.2.4, else 1.
"""
import argparse
import glob
import os
import socket
import subprocess
import tempfile
import time
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=8)
        self.s.settimeout(8)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
        i = self.b.index(b"\r\n")
        ln = self.b[:i]
        self.b = self.b[i + 2:]
        return ln

    def rd(self):
        ln = self._line()
        t = ln[:1]
        if t in (b"+", b"-", b":"):
            return ln[1:]
        if t == b"$":
            n = int(ln[1:])
            if n < 0:
                return None
            while len(self.b) < n + 2:
                self.b += self.s.recv(65536)
            d = self.b[:n]
            self.b = self.b[n + 2:]
            return d
        if t == b"*":
            n = int(ln[1:])
            if n < 0:
                return None
            return [self.rd() for _ in range(n)]
        return ln

    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else (str(x).encode() if isinstance(x, int) else x)
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o)
        return self.rd()


def parse_aof(data):
    out = []
    i = 0
    n = len(data)
    while i < n:
        if data[i:i + 1] != b"*":
            j = data.find(b"\r\n", i)
            if j < 0:
                break
            i = j + 2
            continue
        j = data.find(b"\r\n", i)
        cnt = int(data[i + 1:j])
        i = j + 2
        args = []
        ok = True
        for _ in range(cnt):
            if data[i:i + 1] != b"$":
                ok = False
                break
            j = data.find(b"\r\n", i)
            ln = int(data[i + 1:j])
            i = j + 2
            args.append(data[i:i + ln])
            i = i + ln + 2
        if ok and args and args[0].upper() not in (b"SELECT", b"MULTI", b"EXEC", b"HELLO"):
            out.append(args)
    return out


def wait_up(port):
    for _ in range(60):
        try:
            if Conn(port).cmd("PING") == b"PONG":
                return True
        except OSError:
            time.sleep(0.2)
    return False


def run(binpath, is_redis, scenario):
    d = tempfile.mkdtemp(prefix="exp_prop_")
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    if is_redis:
        argv = [binpath, "--port", str(port), "--dir", d, "--save", "",
                "--appendonly", "yes", "--appendfsync", "always", "--enable-debug-command", "yes"]
    else:
        argv = [binpath, "--port", str(port), "--aof", os.path.join(d, "append.aof"),
                "--enable-debug-command", "yes"]
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    spop_reply = None
    try:
        if not wait_up(port):
            return None, None
        c = Conn(port)
        if not is_redis:
            c.cmd("CONFIG", "SET", "appendfsync", "always")
        if scenario == "lazy_expire":
            c.cmd("SET", "k", "v", "PX", "40")
            time.sleep(0.12)
            c.cmd("GET", "k")
        elif scenario == "active_expire":
            c.cmd("SET", "ke", "v", "PX", "40")
            time.sleep(0.8)
            c.cmd("PING")
        elif scenario == "partial_spop":
            c.cmd("SADD", "s", "a", "b", "c", "d", "e")
            spop_reply = c.cmd("SPOP", "s", "2")
        time.sleep(0.5)
        files = (glob.glob(os.path.join(d, "appendonlydir", "*incr*.aof"))
                 + glob.glob(os.path.join(d, "*incr*.aof")))
        data = b""
        for f in sorted(files):
            data += open(f, "rb").read()
        return parse_aof(data), spop_reply
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("FR_BIN", "/data/tmp/cargo-target/release/frankenredis"))
    ap.add_argument("--redis-bin", default=os.environ.get(
        "REDIS_BIN", os.path.join(HERE, "..", "legacy_redis_code/redis/src/redis-server")))
    args = ap.parse_args()
    fr = os.path.abspath(args.bin)
    redis = os.path.abspath(args.redis_bin)
    fails = 0

    for scen, key in [("lazy_expire", b"k"), ("active_expire", b"ke")]:
        ra, _ = run(redis, True, scen)
        fa, _ = run(fr, False, scen)
        if ra is None or fa is None:
            print("FAIL [%s]: a server did not start" % scen)
            fails += 1
            continue
        rexp = [a[0].upper() for a in ra if a[0].upper() in (b"DEL", b"UNLINK") and key in a[1:]]
        fexp = [a[0].upper() for a in fa if a[0].upper() in (b"DEL", b"UNLINK") and key in a[1:]]
        if rexp == fexp and rexp:
            print("OK   [%s] propagated %r for expired %s" % (scen, [x.decode() for x in rexp], key.decode()))
        else:
            print("FAIL [%s] redis=%r fr=%r" % (scen, rexp, fexp))
            fails += 1

    ra, rs = run(redis, True, "partial_spop")
    fa, fs = run(fr, False, "partial_spop")
    for name, aof, reply in [("redis", ra, rs), ("fr", fa, fs)]:
        prop = [a for a in (aof or []) if a[0].upper() in (b"SREM", b"DEL", b"SPOP") and len(a) > 1 and a[1] == b"s"]
        if not prop:
            print("FAIL [partial_spop:%s] no propagation" % name)
            fails += 1
            continue
        cmd = prop[0][0].upper()
        if cmd != b"SREM":
            print("FAIL [partial_spop:%s] propagated %s, expected SREM" % (name, cmd.decode()))
            fails += 1
            continue
        prop_members = set(prop[0][2:])
        reply_members = set(reply or [])
        if prop_members == reply_members:
            print("OK   [partial_spop:%s] SREM s %r == SPOP reply" % (name, [m.decode() for m in prop_members]))
        else:
            print("FAIL [partial_spop:%s] SREM members %r != SPOP reply %r" % (name, prop_members, reply_members))
            fails += 1

    if fails:
        print("\n%d propagation check(s) FAILED" % fails)
        sys.exit(1)
    print("\nOK: expiry (lazy+active) and partial-SPOP propagation forms match redis 7.2.4")
    sys.exit(0)


if __name__ == "__main__":
    main()
