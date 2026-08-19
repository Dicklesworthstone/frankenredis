#!/usr/bin/env python3
"""Differential: what happens to writes when the disk-error (MISCONF) gate is up?

frankenredis-miscobey-2160s was filed SOURCE-VERIFIED and explicitly not reproduced, claiming a
MISCONF'd REPLICA answers MISCONF to its master's replicated writes and silently drops them.
This reproduces the scenario against both engines and answers it.

TWO SETUP FACTS THAT MAKE OR BREAK THE EXPERIMENT, both learned by having a control rather than
by reading, and both of which silently produce a vacuous "no divergence" if missed:

  * upstream's `writeCommandsDeniedByDiskError` (server.c:4473) requires `saveparamslen > 0`.
    Start a server with `--save ""` and the disk-error gate is DISABLED, so a failed BGSAVE
    denies nothing at all. Both engines must be started WITH a save point.
  * `CONFIG SET dir` is a PROTECTED config on both engines and is refused, so the RDB target is
    broken by making the server's own data directory unwritable. That requires not running as
    root -- root bypasses the mode bits and gives a cheerfully successful BGSAVE.

LEG A (standalone) exists to prove the state is inducible and the gate REACHABLE. Without it a
quiet leg B means nothing. LEG B is the bead's actual scenario.

Upstream's obeyed-client arm (server.c:4027) panics unless `replica-ignore-disk-write-errors` is
yes -- note the config name, which is NOT the `repl_ignore_disk_write_error` field name in
server.h. So three leg-B outcomes are distinguishable and each means something different:

    APPLIED    the master's write is on the replica     (upstream's ignore=yes arm)
    PANICKED   the replica process is GONE              (upstream's DEFAULT arm)
    DROPPED    alive, still linked, data MISSING        (what the bead predicted)

Exit 0 if fr's replica does not DROP, 1 if it does, 2 on a harness failure.
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


def enc(argv):
    out = b"*%d\r\n" % len(argv)
    for a in argv:
        b = a.encode() if isinstance(a, str) else a
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


class Conn:
    def __init__(self, port, timeout=10.0):
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


def start(binary, port, workdir, name, extra=()):
    os.makedirs(workdir, exist_ok=True)
    argv = [str(binary), "--port", str(port), "--dir", workdir, "--save", "900 1", *extra]
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, cwd=workdir)
    deadline = time.time() + 20.0
    while time.time() < deadline:
        # Bounded wait plus a liveness check on the writer: a port loop that never reads the
        # producer waits forever on one that already exited nonzero.
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


def induce_misconf(conn, datadir):
    if os.geteuid() == 0:
        print("HARNESS FAILURE: running as root would bypass the read-only dir and fake a "
              "successful BGSAVE")
        sys.exit(2)
    os.chmod(datadir, 0o555)
    conn.cmd("CONFIG", "SET", "stop-writes-on-bgsave-error", "yes")
    conn.cmd("BGSAVE")
    ok, _ = wait_for(
        lambda: info_field(conn, "persistence", "rdb_last_bgsave_status") == "err", budget=20.0)
    return ok


def leg_a(engine, binary, port, root):
    d = os.path.join(root, engine, "standalone")
    shutil.rmtree(d, ignore_errors=True)
    proc = start(binary, port, d, f"{engine}-standalone")
    out = {}
    try:
        c = Conn(port)
        out["write_before"] = c.cmd("SET", "a:key", "ok")
        out["misconf_induced"] = induce_misconf(c, d)
        out["write_after"] = c.cmd("SET", "b:key", "denied?")
        out["ping_after"] = c.cmd("PING")
        out["read_after"] = c.cmd("GET", "a:key")
        c.close()
    finally:
        os.chmod(d, 0o755)
        proc.terminate()
        try:
            proc.wait(timeout=6)
        except subprocess.TimeoutExpired:
            proc.kill()
    return out


def leg_b(engine, binary, mport, rport, root, extra_replica=()):
    base = os.path.join(root, engine, "repl")
    shutil.rmtree(base, ignore_errors=True)
    mdir, rdir = os.path.join(base, "m"), os.path.join(base, "r")
    master = start(binary, mport, mdir, f"{engine}-master")
    replica = start(binary, rport, rdir, f"{engine}-replica", extra_replica)
    out = {}
    try:
        m, r = Conn(mport), Conn(rport)
        r.cmd("REPLICAOF", "127.0.0.1", str(mport))
        ok, _ = wait_for(
            lambda: info_field(r, "replication", "master_link_status") == "up", budget=25.0)
        out["link_up"] = ok
        if not ok:
            out["VERDICT"] = "INCONCLUSIVE -- replica never synced"
            return out
        m.cmd("SET", "pre:key", "before")
        out["repl_works_before"] = wait_for(lambda: r.cmd("GET", "pre:key") == "before")[0]
        out["misconf_induced"] = induce_misconf(r, rdir)
        out["master_write"] = m.cmd("SET", "probe:key", "AFTER_MISCONF")
        applied, _ = wait_for(lambda: r.cmd("GET", "probe:key") == "AFTER_MISCONF", budget=12.0)
        out["replica_applied"] = applied
        alive = replica.poll() is None
        out["replica_alive"] = alive
        if alive:
            try:
                r2 = Conn(rport)
                out["replica_ping"] = r2.cmd("PING")
                out["link_after"] = info_field(r2, "replication", "master_link_status")
                r2.close()
            except OSError as e:
                out["replica_ping"] = f"<unreachable {e}>"
        else:
            out["replica_exit_rc"] = replica.returncode
        if not out.get("misconf_induced"):
            out["VERDICT"] = "INCONCLUSIVE -- MISCONF never induced"
        elif applied:
            out["VERDICT"] = "APPLIED"
        elif not alive:
            out["VERDICT"] = "PANICKED -- process gone"
        else:
            out["VERDICT"] = "DROPPED -- alive, linked, data missing"
        m.close()
    finally:
        os.chmod(rdir, 0o755)
        for p in (master, replica):
            if p.poll() is None:
                p.terminate()
                try:
                    p.wait(timeout=6)
                except subprocess.TimeoutExpired:
                    p.kill()
    return out


def show(label, res):
    print(f"  {label}")
    for k, v in res.items():
        text = repr(v)
        if len(text) > 150:
            text = text[:147] + "...'"
        print(f"      {k:20s} {text}")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fr", type=Path, default=FR_DEFAULT)
    ap.add_argument("--redis", type=Path, default=REDIS_DEFAULT)
    ap.add_argument("--base-port", type=int, default=28951)
    args = ap.parse_args()

    for label, path in (("fr", args.fr), ("redis", args.redis)):
        if not path.exists():
            print(f"SKIP: {label} binary not built at {path}")
            return 0

    p = args.base_port
    root = tempfile.mkdtemp(prefix="misconf_repro_")
    try:
        print("LEG A -- standalone: is MISCONF inducible, and is the gate reachable?")
        a_redis = leg_a("redis", args.redis, p, root)
        a_fr = leg_a("fr", args.fr, p + 1, root)
        show("redis 7.2.4", a_redis)
        show("fr", a_fr)

        print("\nLEG B -- replica under MISCONF, master writes")
        b_default = leg_b("redis-default", args.redis, p + 10, p + 11, root)
        b_ignore = leg_b("redis-ignore", args.redis, p + 20, p + 21, root,
                         ("--replica-ignore-disk-write-errors", "yes"))
        b_fr = leg_b("fr", args.fr, p + 30, p + 31, root)
        show("redis 7.2.4 (default -> panic arm)", b_default)
        show("redis 7.2.4 (ignore=yes -> apply arm)", b_ignore)
        show("fr", b_fr)
    finally:
        shutil.rmtree(root, ignore_errors=True)

    print()
    if not (a_redis.get("misconf_induced") and a_fr.get("misconf_induced")):
        print("INCONCLUSIVE: MISCONF was not induced on both engines; every row above is vacuous.")
        return 2
    rc = 0
    if b_fr.get("VERDICT", "").startswith("DROPPED"):
        print("REPRODUCED: fr's replica DROPPED its master's write while staying alive and linked.")
        rc = 1
    else:
        print(f"NOT REPRODUCED: fr's replica verdict is {b_fr.get('VERDICT')!r}; "
              f"upstream default is {b_default.get('VERDICT')!r} and "
              f"upstream ignore=yes is {b_ignore.get('VERDICT')!r}.")
    if a_redis.get("ping_after") != a_fr.get("ping_after"):
        print("SEPARATE DIVERGENCE -- PING under MISCONF differs. Upstream includes pingCommand "
              "in the denial set (server.c:4025); fr answers it normally.")
        print(f"    redis  {str(a_redis.get('ping_after'))[:60]!r}")
        print(f"    fr     {a_fr.get('ping_after')!r}")
    return rc


if __name__ == "__main__":
    sys.exit(main())
