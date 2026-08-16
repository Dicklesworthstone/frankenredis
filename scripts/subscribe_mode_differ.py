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
import re
import socket
import subprocess
import tempfile
import sys
import time


def free_port():
    """(frankenredis-83tve) Ports 21870/21871 were hardcoded. A dozen agents share
    this box and ip_local_port_range covers the whole space, so those numbers can
    be taken by anyone. The dangerous case is not a failed bind but a SUCCESSFUL
    connection: launch() below only PINGed the port, so any squatter that speaks
    RESP — very often another agent's redis or fr, since that is what this box
    runs — answered PONG and silently became the engine under test. Ephemeral
    ports plus the pid check in launch() close that."""
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def server_pid(port):
    """process_id from INFO server, over a raw socket so this stays independent of
    whatever reply shape the local Conn class returns."""
    s = socket.create_connection(("127.0.0.1", port), 3)
    s.settimeout(4.0)
    try:
        s.sendall(b"*2\r\n$4\r\nINFO\r\n$6\r\nserver\r\n")
        buf = b""
        while b"process_id:" not in buf:
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
        m = re.search(rb"process_id:(\d+)", buf)
        return int(m.group(1)) if m else None
    finally:
        s.close()


REDIS_PORT = free_port()
FR_PORT = free_port()


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


def launch(cmdline, port, cwd=None):
    proc = subprocess.Popen(cmdline, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, cwd=cwd,
                            start_new_session=True)
    for _ in range(80):
        try:
            c = Conn(port)
            if c.cmd("PING") == ("simple", b"PONG"):
                c.close()
                # (frankenredis-83tve) PONG only proves SOMETHING listens. Pin the
                # answering process to the one we launched, or a squatter silently
                # becomes the engine under test.
                pid = server_pid(port)
                if pid != proc.pid:
                    proc.kill()
                    raise SystemExit(
                        f"port {port} is served by pid {pid}, not the server we "
                        f"launched (pid {proc.pid}): another process holds the "
                        f"port, so any parity verdict here would describe its "
                        f"server, not {cmdline[0]}")
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
    # (frankenredis-83tve) The engines are launched with cwd=workdir below, so a
    # RELATIVE binary path would resolve against that temp dir and vanish -- and
    # run_parity_differs.sh passes exactly that by default.
    binpath = os.path.abspath(binpath) if binpath else binpath
    redispath = os.path.abspath(redispath) if redispath else redispath
    if not binpath or not os.path.exists(binpath):
        print("FAIL: frankenredis binary not found (pass --bin PATH)", file=sys.stderr)
        sys.exit(2)
    if not redispath or not os.path.exists(redispath):
        print("FAIL: redis-server not found (pass --redis-bin PATH)", file=sys.stderr)
        sys.exit(2)

    failures = []
    procs = []
    # (frankenredis-83tve) Run BOTH engines in a private cwd and disable saving.
    # Launched bare, they inherit the caller's cwd -- normally the repo root, which
    # a dozen agents share -- so each engine loads whatever dump.rdb is sitting
    # there and writes its own on the way out. That is not hypothetical: an fr-
    # written dump.rdb in the repo root carrying a FUNCTION library redis 7.2.4
    # refuses ("Error registering functions: ERR user_function:3: attempt to index
    # local 't' (a nil value)") made redis abort at startup, which this differ
    # reported as "server did not start". A differ must not inherit state from
    # whoever ran last in the same directory. Isolation is via cwd rather than
    # --dir because redis honours --dir as a startup arg but fr runs command-
    # line args through its protected-config gate and refuses it (frankenredis-fyi51).
    workdir = tempfile.mkdtemp(prefix="fr_subscribe_differ_")
    try:
        procs.append(launch([redispath, "--port", str(REDIS_PORT), "--save", "",
                             "--appendonly", "no"], REDIS_PORT, cwd=workdir))
        procs.append(launch([binpath, "--port", str(FR_PORT), "--save", "",
                             "--appendonly", "no"], FR_PORT, cwd=workdir))

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
