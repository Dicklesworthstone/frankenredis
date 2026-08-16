#!/usr/bin/env python3
"""Self-launching MULTI/EXEC/WATCH transaction-semantics differential gate.

Transactions span the fr-runtime command-queueing / EXECABORT classification and
the fr-store WATCH key-fingerprint comparison — an intricate, regression-prone
surface that no existing differ covers as a unit (pubsub/replication multi-conn
gates are unrelated). This compares fr vs vendored redis 7.2.4 byte-for-byte over:

  * single-connection: queue + EXEC result arrays, EXECABORT on a queued
    unknown / wrong-arity command, inline errors for runtime failures inside
    EXEC, DISCARD, empty EXEC, EXEC/DISCARD without MULTI, nested MULTI,
    WATCH inside MULTI, and the pub/sub family queued in MULTI;
  * multi-connection WATCH races: a watched key modified by another client
    aborts EXEC (nil array); UNWATCH / no-modification let it run; a watched
    key created or deleted by another client also aborts.

Exit 0 if byte-exact, else 1.
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
    """(frankenredis-83tve) Ports 21880/21881 were hardcoded. A dozen agents share
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
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n)
        if t == b":":
            return ("int", int(r))
        if t == b"+":
            return ("ok", r)
        if t == b"-":
            return ("err", r)
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
            if c.cmd("PING") == ("ok", b"PONG"):
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


# --- single-connection scenarios: a list of commands run in order on ONE fresh
# connection; the full reply sequence is compared. ---
def scenario_basic_exec(c):
    return [c.cmd("MULTI"), c.cmd("SET", "k", "1"), c.cmd("INCR", "k"),
            c.cmd("GET", "k"), c.cmd("EXEC")]


def scenario_execabort_unknown(c):
    return [c.cmd("MULTI"), c.cmd("NOSUCHCMD", "a"), c.cmd("SET", "k", "9"),
            c.cmd("EXEC")]


def scenario_execabort_arity(c):
    return [c.cmd("MULTI"), c.cmd("GET"), c.cmd("SET", "k", "9"), c.cmd("EXEC")]


def scenario_inline_runtime_error(c):
    # wrong-type op queues fine, fails at EXEC time but does NOT abort the txn
    c.cmd("DEL", "lk")
    c.cmd("RPUSH", "lk", "a")
    return [c.cmd("MULTI"), c.cmd("SET", "sk", "v"), c.cmd("INCR", "lk"),
            c.cmd("GET", "sk"), c.cmd("EXEC")]


def scenario_discard(c):
    return [c.cmd("MULTI"), c.cmd("SET", "dk", "1"), c.cmd("DISCARD"),
            c.cmd("EXISTS", "dk_nope"), c.cmd("EXEC")]


def scenario_empty_exec(c):
    return [c.cmd("MULTI"), c.cmd("EXEC")]


def scenario_exec_without_multi(c):
    return [c.cmd("EXEC")]


def scenario_discard_without_multi(c):
    return [c.cmd("DISCARD")]


def scenario_nested_multi(c):
    return [c.cmd("MULTI"), c.cmd("MULTI"), c.cmd("SET", "nk", "1"),
            c.cmd("EXEC")]


def scenario_watch_inside_multi(c):
    return [c.cmd("MULTI"), c.cmd("WATCH", "wk"), c.cmd("SET", "wk", "1"),
            c.cmd("EXEC")]


def scenario_subscribe_queued(c):
    # SUBSCRIBE has no CMD_NO_MULTI flag, so it queues; EXEC runs it.
    out = [c.cmd("MULTI"), c.cmd("SUBSCRIBE", "ch"), c.cmd("EXEC")]
    c.cmd("UNSUBSCRIBE", "ch")
    return out


def scenario_no_multi_save_aborts(c):
    # SAVE is CMD_NO_MULTI -> immediate error + flags the txn for EXECABORT.
    return [c.cmd("MULTI"), c.cmd("SAVE"), c.cmd("SET", "k", "1"), c.cmd("EXEC")]


SINGLE_SCENARIOS = [
    ("basic-exec", scenario_basic_exec),
    ("execabort-unknown", scenario_execabort_unknown),
    ("execabort-arity", scenario_execabort_arity),
    ("inline-runtime-error", scenario_inline_runtime_error),
    ("discard", scenario_discard),
    ("empty-exec", scenario_empty_exec),
    ("exec-without-multi", scenario_exec_without_multi),
    ("discard-without-multi", scenario_discard_without_multi),
    ("nested-multi", scenario_nested_multi),
    ("watch-inside-multi", scenario_watch_inside_multi),
    ("subscribe-queued", scenario_subscribe_queued),
    ("no-multi-save-aborts", scenario_no_multi_save_aborts),
]


# --- multi-connection WATCH scenarios: (a_cmds_pre, b_cmds, a_cmds_post). The
# combined reply sequence (A-pre then A-post; B replies are not compared since
# B is identical on both servers) is compared. ---
def run_watch_scenario(port, name):
    a = Conn(port)
    b = Conn(port)
    try:
        a.cmd("DEL", "wk")
        if name == "modified-aborts":
            a.cmd("SET", "wk", "1")
            r = [a.cmd("WATCH", "wk"), a.cmd("MULTI"), a.cmd("GET", "wk")]
            b.cmd("SET", "wk", "2")  # other client modifies the watched key
            r.append(a.cmd("EXEC"))  # -> nil (aborted)
            return r
        if name == "unmodified-runs":
            a.cmd("SET", "wk", "1")
            r = [a.cmd("WATCH", "wk"), a.cmd("MULTI"), a.cmd("GET", "wk"),
                 a.cmd("EXEC")]  # -> [b"1"]
            return r
        if name == "unwatch-then-modify-runs":
            a.cmd("SET", "wk", "1")
            r = [a.cmd("WATCH", "wk"), a.cmd("UNWATCH"), a.cmd("MULTI"),
                 a.cmd("GET", "wk")]
            b.cmd("SET", "wk", "2")
            r.append(a.cmd("EXEC"))  # -> runs, [b"2"]
            return r
        if name == "watch-create-aborts":
            r = [a.cmd("WATCH", "wk"), a.cmd("MULTI"), a.cmd("EXISTS", "wk")]
            b.cmd("SET", "wk", "1")  # creating a watched (absent) key counts
            r.append(a.cmd("EXEC"))  # -> nil
            return r
        if name == "watch-delete-aborts":
            a.cmd("SET", "wk", "1")
            r = [a.cmd("WATCH", "wk"), a.cmd("MULTI"), a.cmd("GET", "wk")]
            b.cmd("DEL", "wk")
            r.append(a.cmd("EXEC"))  # -> nil
            return r
        if name == "watch-same-value-rewrite-aborts":
            # redis aborts on any touch even if the value is unchanged
            a.cmd("SET", "wk", "1")
            r = [a.cmd("WATCH", "wk"), a.cmd("MULTI"), a.cmd("GET", "wk")]
            b.cmd("SET", "wk", "1")
            r.append(a.cmd("EXEC"))  # -> nil
            return r
        raise ValueError(name)
    finally:
        a.close()
        b.close()


WATCH_SCENARIOS = [
    "modified-aborts",
    "unmodified-runs",
    "unwatch-then-modify-runs",
    "watch-create-aborts",
    "watch-delete-aborts",
    "watch-same-value-rewrite-aborts",
]


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
    workdir = tempfile.mkdtemp(prefix="fr_txn_differ_")
    try:
        procs.append(launch([redispath, "--port", str(REDIS_PORT), "--save", "",
                             "--appendonly", "no"], REDIS_PORT, cwd=workdir))
        procs.append(launch([binpath, "--port", str(FR_PORT), "--save", "",
                             "--appendonly", "no"], FR_PORT, cwd=workdir))

        for name, fn in SINGLE_SCENARIOS:
            rc = Conn(REDIS_PORT)
            fc = Conn(FR_PORT)
            try:
                r = fn(rc)
                f = fn(fc)
            finally:
                rc.close()
                fc.close()
            if r != f:
                failures.append(f"single/{name}:\n      redis={r}\n      fr   ={f}")

        for name in WATCH_SCENARIOS:
            r = run_watch_scenario(REDIS_PORT, name)
            f = run_watch_scenario(FR_PORT, name)
            if r != f:
                failures.append(f"watch/{name}:\n      redis={r}\n      fr   ={f}")
    finally:
        for p in reversed(procs):
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()

    if failures:
        print("FAIL: transaction-semantics divergences:")
        for fl in failures:
            print(f"  - {fl}")
        sys.exit(1)
    print(f"OK: MULTI/EXEC/WATCH transaction semantics byte-exact vs redis 7.2.4 "
          f"({len(SINGLE_SCENARIOS)} single-conn + {len(WATCH_SCENARIOS)} "
          f"multi-conn WATCH-race scenarios)")
    sys.exit(0)


if __name__ == "__main__":
    main()
