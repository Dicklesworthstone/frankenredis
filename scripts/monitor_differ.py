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
remaining `[db] "CMD" "arg"...` payload is compared. A dedicated addr-fidelity
check separately asserts the `[db addr]` prefix carries the sender's real
local port (the addr cannot be compared fr-vs-redis directly: each sender
opens its own ephemeral port to each server).

ASSERTED (frankenredis-ax9ox, landed): the real client peer address in the
`[db addr]` prefix (not `127.0.0.1:0`), and control-char argument escaping with
the C named escapes (\\n \\r \\t \\a \\b) per sds.c::sdscatrepr.

ASSERTED (frankenredis-e8f9q, landed): SELECT / SWAPDB / WATCH / UNWATCH /
MULTI / EXEC / DISCARD and every command queued inside a MULTI block are now
mirrored, in upstream order (MULTI, then the queued commands as EXEC runs them,
then EXEC), with admin commands (SAVE / DEBUG / CONFIG / ...) still excluded.
Every mirrored line is compared, including the transaction-control lines.

EXCLUDED (frankenredis-ax9ox residual — still open): script-invoked redis.call
commands are not mirrored at all (redis shows them with the `lua` address) ->
not exercised here.
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


def extract_addr(line):
    """`<ts> [<db> <addr>] ...` -> '<addr>' (the client-address field)."""
    if line is None:
        return None
    try:
        head = line.split("]", 1)[0]
        inner = head.split("[", 1)[1]
        return inner.split(" ", 1)[1]
    except (IndexError, ValueError):
        return None


def addr_fidelity(port, mon):
    """Run PING from a fresh sender; return (sender_local_addr, monitor_addr).

    The monitor `[db addr]` prefix must report the sender's real local port
    (frankenredis-ax9ox), not the old `127.0.0.1:0` placeholder. This is checked
    per-server rather than fr-vs-redis because each sender opens its own
    ephemeral port to each server, so the two addresses legitimately differ.
    """
    mon.drain()
    sender = Conn(port)
    local = sender.s.getsockname()
    want = f"{local[0]}:{local[1]}"
    try:
        sender.cmd("PING")
        ln = mon.read_line(timeout=0.8)
    finally:
        sender.close()
    return want, extract_addr(ln)


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
# Args may be bytes to exercise control-char escaping (ax9ox, now asserted).
CASES = [
    ("simple-set", [("SET", "foo", "bar")]),
    ("get", [("GET", "foo")]),
    ("mset", [("MSET", "a", "1", "b", "2")]),
    ("quote-arg", [("SET", "qk", 'has"quote')]),
    ("backslash-arg", [("SET", "qk", "back\\slash")]),
    ("space-arg", [("SET", "qk", "with space")]),
    ("empty-arg", [("SET", "qk", "")]),
    ("binary-printable", [("SET", "qk", "~!@#$%^&*()_+={}|;:<>,.?/")]),
    # control-char escaping (ax9ox): named escapes for \n \r \t \a \b, \xNN else
    ("ctrl-named", [("SET", "ek", b"a\nb\tc\rd\x07e\x08f")]),
    ("ctrl-mixed", [("SET", "ek", b"x\ny\tz\x07\x08\xfe\x00\x1f\x7f")]),
    ("ctrl-highbytes", [("SET", "ek", b"\x00\x01\x1f\x7f\x80\xff")]),
    ("ctrl-quote-and-nl", [("SET", "ek", b'q"\\\n\t')]),
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
    # transaction mirroring (e8f9q): MULTI, queued cmds (during EXEC), EXEC
    ("multi-exec", [("MULTI",), ("SET", "tx", "1"), ("INCR", "tx"), ("EXEC",)]),
    ("multi-discard", [("MULTI",), ("SET", "dx", "1"), ("DISCARD",)]),
    ("watch-unwatch", [("WATCH", "wk"), ("UNWATCH",)]),
    ("swapdb", [("SWAPDB", "0", "1"), ("SWAPDB", "1", "0")]),
    # container subcommand admin resolution (e8f9q): CONFIG GET / SLOWLOG GET /
    # DEBUG are admin -> hidden; ACL WHOAMI / CLIENT ID / COMMAND COUNT /
    # PUBSUB CHANNELS / OBJECT HELP are not -> shown. fr must resolve the
    # `<parent>|<sub>` flags, not the (flag-less) container, to match redis.
    ("config-get-hidden", [("CONFIG", "GET", "maxmemory")]),
    ("slowlog-get-hidden", [("SLOWLOG", "GET")]),
    ("debug-hidden", [("DEBUG", "SET-ACTIVE-EXPIRE", "1")]),
    ("acl-whoami-shown", [("ACL", "WHOAMI")]),
    ("client-id-shown", [("CLIENT", "ID")]),
    ("command-count-shown", [("COMMAND", "COUNT")]),
    ("pubsub-channels-shown", [("PUBSUB", "CHANNELS")]),
    ("object-help-shown", [("OBJECT", "HELP")]),
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
    # Read every mirrored line that arrives within the window. SELECT / MULTI /
    # EXEC and queued transaction commands are now mirrored (frankenredis-e8f9q),
    # so every line is compared — including the db-prefix a preceding SELECT
    # established.
    lines = []
    while True:
        ln = mon.read_line(timeout=0.6)
        if ln is None:
            break
        lines.append(normalize(ln))
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

        # addr-fidelity (ax9ox): the [db addr] prefix must carry the sender's
        # real local port on BOTH servers, never the 127.0.0.1:0 placeholder.
        for name, port, mon in (("fr", FR_PORT, fmon), ("redis", REDIS_PORT, rmon)):
            want, got = addr_fidelity(port, mon)
            if got != want:
                failures.append(
                    f"addr-fidelity[{name}]: monitor addr {got!r}, "
                    f"want sender's real addr {want!r}")
            if got == "127.0.0.1:0":
                failures.append(
                    f"addr-fidelity[{name}]: addr is the 127.0.0.1:0 placeholder")
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
          f"({len(CASES)} cases + addr-fidelity; real peer addr, control-char "
          f"escaping [ax9ox] and SELECT/MULTI/EXEC/queued mirroring [e8f9q] all "
          f"asserted; lua-feed still excluded [ax9ox residual])")
    sys.exit(0)


if __name__ == "__main__":
    main()
