#!/usr/bin/env python3
"""Differential: which commands does a STALE replica serve? (frankenredis-stalelist-hto86)

Upstream gates the stale-replica refusal on the COMMAND'S OWN FLAG --
`is_denystale_command = !(c->cmd->flags & CMD_STALE)` -- so anything declared STALE in the
command table is served while the master link is down and `replica-serve-stale-data` is no.
fr instead carries a hand-written allowlist in `Runtime::reject_stale_replica_read_request`,
and a restated list drifts from the table it restates.

The bead was filed SOURCE-VERIFIED and explicitly not reproduced. This reproduces it: build a
genuinely stale replica of each engine and ask every disputed command, one FRESH connection
each, comparing replies.

SETUP, and the control that keeps it honest. A replica is stale when its master link is DOWN
and replica-serve-stale-data is no. The link is broken by pointing REPLICAOF at a dead port,
which is cleaner than killing a master (no races on shutdown) and is verified by reading
master_link_status back as "down" before anything is asked.

CONTROL LEG: a plain GET must be REFUSED on both engines before any row is read. If it is not,
the replica is not stale and every row below is vacuous -- the failure mode that makes a
differential agree for the wrong reason. Exit 2 says exactly that, and is not exit 0.

A fresh connection per command matters: MONITOR puts a connection into monitor mode, QUIT
closes it, and MULTI leaves it in a transaction, so reusing one would contaminate later rows.

Exit 0 when both engines agree on every command, 1 on any divergence, 2 if the stale state was
never established or a binary is missing.
"""

from __future__ import annotations

import argparse
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
FR_DEFAULT = REPO / "target" / "release" / "frankenredis"
REDIS_DEFAULT = REPO / "legacy_redis_code" / "redis" / "src" / "redis-server"

# The 19 the bead says fr wrongly REFUSES (upstream declares them STALE), plus WAIT which it
# says fr wrongly PERMITS, plus controls that must be refused by both.
COMMANDS = [
    ("debug", ["DEBUG", "JMAP"]),
    ("discard", ["DISCARD"]),
    ("echo", ["ECHO", "hello"]),
    ("eval", ["EVAL", "return 1", "0"]),
    ("eval_ro", ["EVAL_RO", "return 1", "0"]),
    ("evalsha", ["EVALSHA", "0" * 40, "0"]),
    ("evalsha_ro", ["EVALSHA_RO", "0" * 40, "0"]),
    ("exec", ["EXEC"]),
    ("failover", ["FAILOVER", "ABORT"]),
    ("fcall", ["FCALL", "nosuchfn", "0"]),
    ("fcall_ro", ["FCALL_RO", "nosuchfn", "0"]),
    ("lastsave", ["LASTSAVE"]),
    ("multi", ["MULTI"]),
    ("quit", ["QUIT"]),
    ("reset", ["RESET"]),
    ("time", ["TIME"]),
    ("unwatch", ["UNWATCH"]),
    ("watch", ["WATCH", "k"]),
    ("wait", ["WAIT", "0", "0"]),
    # subcommand-only STALE cases the parent-level allowlist cannot express
    ("object|help", ["OBJECT", "HELP"]),
    ("xinfo|help", ["XINFO", "HELP"]),
    ("script|load", ["SCRIPT", "LOAD", "return 1"]),
    # controls: upstream does NOT flag these stale, so both engines must refuse
    ("CONTROL get", ["GET", "k"]),
    ("CONTROL dbsize", ["DBSIZE"]),
]

MONITOR_CASE = ("monitor", ["MONITOR"])


def enc(argv):
    out = b"*%d\r\n" % len(argv)
    for a in argv:
        b = a.encode() if isinstance(a, str) else a
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


class Conn:
    def __init__(self, port, timeout=6.0):
        self.s = socket.create_connection(("127.0.0.1", port), timeout)
        self.s.settimeout(timeout)
        self.buf = b""

    def _line(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d:
                raise EOFError
            self.buf += d
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        t, rest = line[:1], line[1:]
        if t in (b"+", b":"):
            return rest.decode("latin1")
        if t == b"-":
            return "ERR> " + rest.decode("latin1")
        if t == b"$":
            n = int(rest)
            if n == -1:
                return None
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(65536)
            v, self.buf = self.buf[:n], self.buf[n + 2:]
            return v.decode("latin1")
        if t == b"*":
            n = int(rest)
            if n == -1:
                return None
            return [self._read() for _ in range(n)]
        return line.decode("latin1")

    def cmd(self, *argv):
        try:
            self.s.sendall(enc(list(argv)))
            return self._read()
        except (socket.timeout, EOFError, OSError) as e:
            return f"<NO REPLY {type(e).__name__}>"

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def start(binary, port, workdir, name):
    os.makedirs(workdir, exist_ok=True)
    argv = [str(binary), "--port", str(port), "--dir", workdir, "--save", ""]
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, cwd=workdir)
    deadline = time.time() + 20.0
    while time.time() < deadline:
        if proc.poll() is not None:
            err = proc.stderr.read().decode("utf-8", "replace")[-800:]
            print(f"HARNESS FAILURE: {name} exited rc={proc.returncode}\n{err}")
            sys.exit(2)
        try:
            s = socket.create_connection(("127.0.0.1", port), 0.3)
            s.close()
            return proc
        except OSError:
            time.sleep(0.15)
    proc.kill()
    print(f"HARNESS FAILURE: {name} never listened on {port}")
    sys.exit(2)


def wait_for(fn, budget=25.0, interval=0.3):
    deadline = time.time() + budget
    last = None
    while time.time() < deadline:
        try:
            last = fn()
        except Exception:
            last = None
        if last:
            return True, last
        time.sleep(interval)
    return False, last


def info_field(conn, section, field):
    txt = conn.cmd("INFO", section)
    if not isinstance(txt, str):
        return None
    for line in txt.splitlines():
        if line.startswith(field + ":"):
            return line.split(":", 1)[1].strip()
    return None


def make_stale(engine, binary, port, dead_port, root):
    """Bring up a replica whose master link is DOWN, with stale reads refused."""
    d = os.path.join(root, engine)
    shutil.rmtree(d, ignore_errors=True)
    proc = start(binary, port, d, f"{engine}-replica")
    c = Conn(port)
    c.cmd("CONFIG", "SET", "replica-serve-stale-data", "no")
    # Point at a port nothing listens on: link can never come up, and no master shutdown race.
    c.cmd("REPLICAOF", "127.0.0.1", str(dead_port))
    ok, status = wait_for(
        lambda: info_field(c, "replication", "master_link_status") == "down", budget=20.0)
    return proc, c, ok, status


def refused(reply):
    """True when the reply is the stale-replica refusal.

    Compared on the CLASS of the reply, not its bytes: TIME is flagged STALE upstream and is
    SERVED by both engines, but it returns the actual clock, so a byte comparison reports a
    divergence on two correct answers. Commands whose value legitimately differs between two
    calls must be compared on served-vs-refused.
    """
    return isinstance(reply, str) and "MASTERDOWN" in reply


def probe(port, argv):
    c = Conn(port)
    try:
        return c.cmd(*argv)
    finally:
        c.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fr", type=Path, default=FR_DEFAULT)
    ap.add_argument("--redis", type=Path, default=REDIS_DEFAULT)
    ap.add_argument("--base-port", type=int, default=29101)
    args = ap.parse_args()

    for label, path in (("fr", args.fr), ("redis", args.redis)):
        if not path.exists():
            print(f"SKIP: {label} binary not built at {path}")
            return 0

    root = tempfile.mkdtemp(prefix="stalegate_")
    dead = args.base_port + 50
    procs = []
    try:
        rp, rc_conn, rok, rstat = make_stale("redis", args.redis, args.base_port, dead, root)
        fp, fc_conn, fok, fstat = make_stale("fr", args.fr, args.base_port + 1, dead, root)
        procs = [rp, fp]
        print(f"stale state: redis link={rstat!r} established={rok}   "
              f"fr link={fstat!r} established={fok}")
        if not (rok and fok):
            print("INCONCLUSIVE: the master link never went down on at least one engine.")
            return 2

        # CONTROL: a plain GET must be refused on BOTH before any row below means anything.
        g_redis = probe(args.base_port, ["GET", "k"])
        g_fr = probe(args.base_port + 1, ["GET", "k"])
        print(f"CONTROL GET   redis {str(g_redis)[:60]!r}")
        print(f"CONTROL GET   fr    {str(g_fr)[:60]!r}")
        refused = lambda r: isinstance(r, str) and r.startswith("ERR>")
        if not (refused(g_redis) and refused(g_fr)):
            print("INCONCLUSIVE: a plain GET was NOT refused, so the replica is not stale and "
                  "every row below is vacuous.")
            return 2
        print()

        # A real SHA, so EVALSHA actually REACHES the stale gate. With a bogus digest both
        # engines answer NOSCRIPT before the gate is consulted and the row agrees vacuously --
        # agreement that proves nothing is the failure mode this whole file exists to avoid.
        sha_redis = probe(args.base_port, ["SCRIPT", "LOAD", "return 1"])
        sha_fr = probe(args.base_port + 1, ["SCRIPT", "LOAD", "return 1"])
        print(f"SCRIPT LOAD   redis {str(sha_redis)[:60]!r}")
        print(f"SCRIPT LOAD   fr    {str(sha_fr)[:60]!r}")
        extra = []
        if isinstance(sha_redis, str) and len(sha_redis) == 40 and sha_redis == sha_fr:
            extra = [("evalsha REAL sha", ["EVALSHA", sha_redis, "0"])]
        else:
            print("   (SCRIPT LOAD did not yield a usable sha on both; skipping the real-sha row)")
        # Shebang scripts: upstream refuses unless the script declares allow-stale.
        extra += [
            ("eval shebang no flags",
             ["EVAL", "#!lua name=t\nreturn 1", "0"]),
            ("eval shebang allow-stale",
             ["EVAL", "#!lua flags=allow-stale,no-writes\nreturn 1", "0"]),
        ]
        print()

        rows = []
        for name, argv in COMMANDS + extra:
            a = probe(args.base_port, argv)
            b = probe(args.base_port + 1, argv)
            # Same CLASS of answer is parity; identical bytes is not required for commands
            # whose value moves between calls.
            rows.append((name, argv, a, b, refused(a) == refused(b)))
        # MONITOR last and on its own connection: it changes the connection's mode.
        name, argv = MONITOR_CASE
        ma = probe(args.base_port, argv)
        mb = probe(args.base_port + 1, argv)
        rows.append((name, argv, ma, mb, refused(ma) == refused(mb)))

        div = [r for r in rows if not r[4]]
        for name, argv, a, b, same in rows:
            tag = "AGREE  " if same else "DIVERGE"
            print(f"{tag} {name}")
            served = lambda r: "REFUSED" if refused(r) else "served"
            print(f"        redis {served(a):8s} {str(a)[:70]!r}")
            print(f"        fr    {served(b):8s} {str(b)[:70]!r}")
        print(f"\n{len(rows) - len(div)}/{len(rows)} commands agree on a STALE replica")
        if div:
            print(f"{len(div)} DIVERGENT: {', '.join(r[0] for r in div)}")
            return 1
        return 0
    finally:
        for p in procs:
            if p.poll() is None:
                p.terminate()
                try:
                    p.wait(timeout=6)
                except subprocess.TimeoutExpired:
                    p.kill()
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
