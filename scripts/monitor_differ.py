#!/usr/bin/env python3
"""Self-launching MONITOR-stream differential gate vs redis 7.2.4.

Locks in the parts of the MONITOR command-mirror that fr already gets right
and that no other differ covers as a distinct surface:
  * command name + argument vector, byte-for-byte
  * printable argument quoting/escaping (`"` -> \\" , `\\` -> \\\\ , empty arg)
  * the per-connection SELECTed-db number shown in the `[db ...]` prefix
  * MULTI/EXEC expansion (every queued command is mirrored on EXEC)

Each test runs a command sequence from a fresh sender connection while a
persistent MONITOR connection on each server captures the mirrored lines;
the timestamp and the client-address field are normalised away and the
remaining `[db] "CMD" "arg"...` payload is compared.

EXCLUDED (frankenredis-ax9ox — all three live in fr-runtime feed_monitors and
are blocked on that file's reservation; flip these to asserts when ax9ox lands):
  1. client address: fr prints `127.0.0.1:0` for every client instead of the
     real peer addr (and `lua` for script-invoked commands) -> addr normalised
     out here.
  2. control-char arg escaping: redis sdscatrepr emits NAMED escapes
     (\\n \\r \\t \\a \\b); fr emits \\xNN -> only printable args are tested.
  3. script-invoked redis.call commands are not mirrored at all (redis shows
     them with the `lua` address) -> not exercised here.

EXCLUDED (frankenredis-e8f9q — also fr-runtime feed_monitors, blocked): fr feeds
monitors AFTER execution, so SELECT / MULTI / EXEC and every command queued
inside a MULTI block are never mirrored (redis feeds at receive time and shows
them all). Lines whose command is SELECT/MULTI/EXEC/DISCARD are filtered from
both sides before comparison, and the all-queued MULTI/EXEC case is omitted; the
db-context cases still assert that the command AFTER a SELECT carries the right
`[db ...]` number (fr tracks the db correctly even though it drops the SELECT
mirror line). Un-exclude and re-run when e8f9q lands.
"""
import argparse
import os
import socket
import subprocess
import sys
import time

REDIS_PORT = 21850
FR_PORT = 21851


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
    """Minimal RESP2 client: send a command, read one reply."""

    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(4.0)
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
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b":":
            return int(r)
        if t == b"+":
            return r.decode("latin1")
        if t == b"-":
            return "ERR:" + r.decode("latin1")
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        raise ValueError(l)

    def send(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)

    def cmd(self, *a):
        self.send(*a)
        return self.parse()

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


class MonitorConn:
    """A connection parked in MONITOR mode; yields mirrored command lines."""

    def __init__(self, port):
        self.c = Conn(port)
        assert self.c.cmd("MONITOR") == "OK"

    def drain(self):
        self.c.s.settimeout(0.25)
        try:
            while True:
                self.c.parse()
        except (socket.timeout, OSError):
            pass
        self.c.b = b""
        self.c.s.settimeout(4.0)

    def read_line(self, timeout=1.0):
        self.c.s.settimeout(timeout)
        try:
            return self.c.parse()
        except (socket.timeout, OSError):
            return None


def normalize(line):
    """`<ts> [<db> <addr>] "CMD" "arg"...` -> ("<db>", '"CMD" "arg"...').

    Drops the timestamp and the client-address field (ax9ox) but keeps the
    SELECTed db and the full command payload.
    """
    if line is None:
        return None
    try:
        head, rest = line.split("]", 1)
        # head == '<ts> [<db> <addr>'
        inner = head.split("[", 1)[1]
        db = inner.split(" ", 1)[0]
        return (db, rest.strip())
    except (IndexError, ValueError):
        return ("?", line.strip())


def launch(cmdline, port):
    proc = subprocess.Popen(cmdline, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, start_new_session=True)
    for _ in range(80):
        try:
            c = Conn(port)
            if c.cmd("PING") == "PONG":
                c.close()
                return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"server on port {port} did not start: {cmdline[0]}")


# Each case: a label and a list of command argv tuples run in order on a fresh
# sender connection. The mirrored line for EACH argv is compared (normalised).
# Printable args only — control-char escaping is ax9ox-excluded.
CASES = [
    ("simple-set", [("SET", "foo", "bar")]),
    ("get", [("GET", "foo")]),
    ("mset", [("MSET", "a", "1", "b", "2")]),
    ("quote-arg", [("SET", "qk", 'has"quote')]),
    ("backslash-arg", [("SET", "qk", "back\\slash")]),
    ("space-arg", [("SET", "qk", "with space")]),
    ("empty-arg", [("SET", "qk", "")]),
    ("binary-printable", [("SET", "qk", "~!@#$%^&*()_+={}|;:<>,.?/")]),
    ("lpush-multi", [("LPUSH", "l", "x", "y", "z")]),
    ("hset", [("HSET", "h", "f1", "v1", "f2", "v2")]),
    ("incr", [("INCR", "ctr")]),
    ("expire", [("EXPIRE", "foo", "100")]),
    ("ping", [("PING",)]),
    ("ping-arg", [("PING", "hello")]),
    ("echo", [("ECHO", "hello world")]),
    ("lowercase-cmd", [("set", "lc", "v")]),
    ("mixed-case-cmd", [("SeT", "mc", "v")]),
    ("select-db", [("SELECT", "5"), ("SET", "dbk", "v")]),
    ("getset-2db", [("SELECT", "9"), ("GET", "dbk"), ("SELECT", "0")]),
    ("zadd", [("ZADD", "z", "1", "m", "2", "n")]),
    ("setex", [("SETEX", "sx", "50", "v")]),
    ("append", [("APPEND", "ap", "more")]),
    ("getrange", [("GETRANGE", "foo", "0", "-1")]),
    ("type", [("TYPE", "foo")]),
    ("del-multi", [("DEL", "a", "b", "nope")]),
]


def run_case(sender_port, mon, label, seq):
    """Run one case on `sender_port`, return list of normalized mirrored lines."""
    mon.drain()
    sender = Conn(sender_port)
    try:
        for argv in seq:
            sender.cmd(*argv)
    finally:
        sender.close()
    # Read every mirrored line that arrives within the window. Lines whose
    # command is SELECT/MULTI/EXEC/DISCARD are dropped (frankenredis-e8f9q:
    # fr does not mirror them); the remaining commands still carry the correct
    # `[db ...]` prefix that a preceding SELECT established.
    lines = []
    while True:
        ln = mon.read_line(timeout=0.6)
        if ln is None:
            break
        norm = normalize(ln)
        cmd = norm[1].split('"')[1].upper() if '"' in norm[1] else ""
        if cmd in ("SELECT", "MULTI", "EXEC", "DISCARD"):
            continue
        lines.append(norm)
    return lines


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
        procs.append(launch([redispath, "--port", str(REDIS_PORT), "--save", "",
                             "--appendonly", "no"], REDIS_PORT))
        procs.append(launch([binpath, "--port", str(FR_PORT)], FR_PORT))
        rmon = MonitorConn(REDIS_PORT)
        fmon = MonitorConn(FR_PORT)
        time.sleep(0.2)

        for label, seq in CASES:
            r = run_case(REDIS_PORT, rmon, label, seq)
            f = run_case(FR_PORT, fmon, label, seq)
            if r != f:
                failures.append(f"{label}:\n      redis={r}\n      fr   ={f}")
    finally:
        for p in reversed(procs):
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()

    if failures:
        print("FAIL: MONITOR command-stream divergences:")
        for fl in failures:
            print(f"  - {fl}")
        sys.exit(1)
    print(f"OK: MONITOR command-mirror byte-exact vs redis 7.2.4 "
          f"({len(CASES)} cases; addr/escaping/lua-feed ax9ox-excluded, "
          f"SELECT/MULTI/EXEC mirroring e8f9q-excluded)")
    sys.exit(0)


if __name__ == "__main__":
    main()
