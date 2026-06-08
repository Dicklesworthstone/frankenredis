#!/usr/bin/env python3
"""Self-launching subscribe-mode command-gate differential gate vs redis 7.2.4.

When a RESP2 client is in subscribe mode, upstream server.c::processCommand only
permits (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET; everything else
is rejected with
  "ERR Can't execute '<fullname>': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE /
   PING / QUIT / RESET are allowed in this context"
where <fullname> is the namespaced container subcommand (e.g. 'config|get').
A RESP3 subscriber has NO such gate (push frames are out-of-band), so it may run
any command. PING in RESP2 subscribe mode returns the 2-element array
["pong", msg] instead of +PONG.

This gate locks that surface in (no other differ covers the command gate — only
message delivery, in pubsub_differ.py). Each case uses a fresh subscribed
connection so subscribe state never couples across cases.

ASSERTED (frankenredis-7tpx0, landed 708db8a17): while subscribed, the upstream
order is arity(incl. resolved subcommand) -> CMD_PROTECTED -> ... -> context
gate, so a known container subcommand with the WRONG argc (CONFIG GET, OBJECT
ENCODING) surfaces its own 'parent|sub' arity error, PING with argc>2 surfaces
the ping arity error, and DEBUG (CMD_PROTECTED) surfaces the protected error —
all BEFORE the context gate.
"""
import argparse
import os
import socket
import subprocess
import sys
import time

REDIS_PORT = 21870
FR_PORT = 21871


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
        if t in (b"$", b"="):
            n = int(r)
            return None if n < 0 else self._rn(n)
        if t == b":":
            return ("int", int(r))
        if t == b"+":
            return ("simple", r)
        if t == b"-":
            return ("err", r)
        if t == b",":
            return ("double", r)
        if t == b"#":
            return ("bool", r)
        if t == b"_":
            return ("null", None)
        if t in (b"*", b"~", b">"):
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        if t == b"%":
            n = int(r)
            return {"map": [(self.parse(), self.parse()) for _ in range(n)]}
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def launch(cmdline, port):
    proc = subprocess.Popen(cmdline, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, start_new_session=True)
    for _ in range(80):
        try:
            c = Conn(port)
            if c.cmd("PING") == ("simple", b"PONG"):
                c.close()
                return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"server on port {port} did not start: {cmdline[0]}")


# RESP2 subscribe-mode cases: subscribe first, then the test command. Replies
# (including the SUBSCRIBE confirmation) are compared byte-for-byte fr-vs-redis.
RESP2_CASES = [
    # allowed pub/sub-control commands
    ("ping-bare", [("PING",)]),
    ("ping-msg", [("PING", "hello")]),
    ("subscribe-more", [("SUBSCRIBE", "beta")]),
    ("unsubscribe-one", [("UNSUBSCRIBE", "beta")]),
    ("psubscribe", [("PSUBSCRIBE", "news.*")]),
    ("punsubscribe-one", [("PUNSUBSCRIBE", "news.*")]),
    ("ssubscribe", [("SSUBSCRIBE", "shard1")]),
    ("sunsubscribe-one", [("SUNSUBSCRIBE", "shard1")]),
    # disallowed valid-arity commands -> namespaced context error
    ("get", [("GET", "k")]),
    ("set", [("SET", "k", "v")]),
    ("incr", [("INCR", "k")]),
    ("lpush", [("LPUSH", "l", "x")]),
    ("hset", [("HSET", "h", "f", "v")]),
    ("config-get-valid", [("CONFIG", "GET", "maxmemory")]),
    ("object-encoding-valid", [("OBJECT", "ENCODING", "k")]),
    ("client-info", [("CLIENT", "INFO")]),
    ("command-count", [("COMMAND", "COUNT")]),
    ("memory-usage", [("MEMORY", "USAGE", "k")]),
    ("slowlog-get", [("SLOWLOG", "GET")]),
    ("acl-whoami", [("ACL", "WHOAMI")]),
    ("cluster-myid", [("CLUSTER", "MYID")]),
    ("echo", [("ECHO", "x")]),
    ("exists", [("EXISTS", "k")]),
    # unknown command -> its own unknown-command error (gate skipped)
    ("unknown-cmd", [("NOSUCHCMD", "a")]),
    # wrong-arity at the PARENT level -> its own arity error (gate skipped)
    ("set-wrong-arity", [("SET", "k")]),
    # (7tpx0) precede the context gate: wrong SUBCOMMAND arity -> own arity
    # error; PING argc>2 -> ping arity error; DEBUG (protected) -> protected err
    ("config-get-wrong-arity", [("CONFIG", "GET")]),
    ("object-encoding-wrong-arity", [("OBJECT", "ENCODING")]),
    ("ping-argc3", [("PING", "a", "b")]),
    ("debug-protected", [("DEBUG", "SLEEP", "0")]),
]

# Commands a RESP3 subscriber may run freely (no gate) — must behave exactly as
# on a non-subscribed connection.
RESP3_CASES = [
    [("GET", "k")],
    [("SET", "k", "v")],
    [("CONFIG", "GET", "maxmemory")],
    [("INCR", "ctr")],
    [("PING",)],
]


def run_resp2_case(port, seq):
    c = Conn(port)
    try:
        sub = c.cmd("SUBSCRIBE", "alpha")
        out = [sub]
        for argv in seq:
            out.append(c.cmd(*argv))
        return out
    finally:
        c.close()


def run_resp3_case(port, seq):
    c = Conn(port)
    try:
        assert isinstance(c.cmd("HELLO", "3"), (dict, list))
        c.cmd("SUBSCRIBE", "alpha")
        return [c.cmd(*argv) for argv in seq]
    finally:
        c.close()


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

        for label, seq in RESP2_CASES:
            r = run_resp2_case(REDIS_PORT, seq)
            f = run_resp2_case(FR_PORT, seq)
            if r != f:
                failures.append(f"RESP2 {label}:\n      redis={r}\n      fr   ={f}")

        for seq in RESP3_CASES:
            r = run_resp3_case(REDIS_PORT, seq)
            f = run_resp3_case(FR_PORT, seq)
            if r != f:
                failures.append(f"RESP3 {seq}:\n      redis={r}\n      fr   ={f}")
    finally:
        for p in reversed(procs):
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()

    if failures:
        print("FAIL: subscribe-mode command-gate divergences:")
        for fl in failures:
            print(f"  - {fl}")
        sys.exit(1)
    print(f"OK: subscribe-mode command gate byte-exact vs redis 7.2.4 "
          f"({len(RESP2_CASES)} RESP2 cases incl. 7tpx0 subcommand-arity / PING "
          f"argc>2 / DEBUG-protected + {len(RESP3_CASES)} RESP3 no-gate cases)")
    sys.exit(0)


if __name__ == "__main__":
    main()
