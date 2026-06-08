#!/usr/bin/env python3
"""Self-launching SENTINEL-mode differential gate vs redis-sentinel 7.2.4.

Locks in frankenredis-w6nhk:
  1. SENTINEL MASTER/MASTERS/SLAVES/SENTINELS reply with a flat array (`*`)
     on a RESP2 connection and a map (`%`) under RESP3 — never a bare `%`
     to a RESP2 client (upstream addReplyMapLen downconverts in RESP2).
  2. Known-subcommand wrong-arity errors name the canonical `sentinel|<sub>`
     pipe fullname (not `sentinel <sub>`).
  3. An unknown subcommand uses the short `unknown subcommand '<x>'. Try
     SENTINEL HELP.` form (commandCheckExistence), not the longer
     "or wrong number of arguments" wording.

Launches a redis master, a redis-sentinel and an fr --sentinel, points both
sentinels at the same master, and compares the deterministic surface.

EXCLUDED (frankenredis-pkdgs — active-monitoring loop not wired): per-master
runtime fields (runid, flags, *ping*, info-refresh, role-reported-time,
down-after-milliseconds), SENTINEL MYID (non-deterministic run-id),
SENTINEL FAILOVER (needs a discovered replica).
"""
import argparse
import os
import socket
import subprocess
import sys
import time

MASTER_PORT = 21840
RSENT_PORT = 21841
FSENT_PORT = 21842


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
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b":":
            return int(r)
        if t == b"+":
            return r.decode()
        if t == b"-":
            return "ERR:" + r.decode()
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        if t == b"%":
            n = int(r)
            return {"%": [(self.parse(), self.parse()) for _ in range(n)]}
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


def reply_type(port, args, hello3=False):
    """First byte of the reply to `args` (RESP3 if hello3)."""
    s = socket.create_connection(("127.0.0.1", port), 3)
    s.settimeout(2.0)
    if hello3:
        s.sendall(b"*2\r\n$5\r\nHELLO\r\n$1\r\n3\r\n")
        time.sleep(0.15)
        s.recv(4096)
    out = b"*%d\r\n" % len(args)
    for x in args:
        x = x if isinstance(x, bytes) else str(x).encode()
        out += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(out)
    time.sleep(0.1)
    d = b""
    try:
        d = s.recv(16)
    except OSError:
        pass
    s.close()
    return d[:1]


def _contains_map(v):
    if isinstance(v, dict):
        return True
    if isinstance(v, list):
        return any(_contains_map(x) for x in v)
    return False


def launch(cmdline, port):
    proc = subprocess.Popen(cmdline, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, start_new_session=True)
    for _ in range(80):
        try:
            c = Conn(port)
            if c.cmd("PING") == "PONG":
                return proc, c
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"server on port {port} did not start: {cmdline[0]}")


# (subcommand-arg-lists) deterministic SENTINEL invocations whose reply must
# match byte-for-byte. Excludes per-master runtime fields / MYID / FAILOVER.
ERROR_CASES = [
    ["SENTINEL", "ZZZ"],
    ["SENTINEL", "NOSUCHSUB"],
    ["SENTINEL", "MASTER"],
    ["SENTINEL", "GET-MASTER-ADDR-BY-NAME"],
    ["SENTINEL", "CKQUORUM"],
    ["SENTINEL", "SLAVES"],
    ["SENTINEL", "REPLICAS"],
    ["SENTINEL", "SENTINELS"],
    ["SENTINEL", "RESET"],
    ["SENTINEL", "IS-MASTER-DOWN-BY-ADDR"],
    ["SENTINEL", "FAILOVER"],
    ["SENTINEL", "REMOVE"],
    ["SENTINEL", "MONITOR"],
    ["SENTINEL", "MASTERS", "extra"],
    ["SENTINEL", "MYID", "extra"],
    ["SENTINEL", "MASTER", "a", "b"],
    ["SENTINEL", "SET", "nomaster", "quorum", "3"],
    ["SENTINEL", "SET", "mymaster", "badparam", "v"],
    ["SENTINEL", "SET", "mymaster", "quorum", "notanum"],
    ["SENTINEL", "GET-MASTER-ADDR-BY-NAME", "mymaster"],
    ["SENTINEL", "CKQUORUM", "nomaster"],
    ["SENTINEL", "REMOVE", "nomaster"],
    ["SENTINEL", "FAILOVER", "nomaster"],
]


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

    conf = f"/tmp/sentinel_differ_{os.getpid()}.conf"
    with open(conf, "w") as f:
        f.write(f"port {RSENT_PORT}\n"
                f"sentinel monitor mymaster 127.0.0.1 {MASTER_PORT} 2\n"
                f"sentinel down-after-milliseconds mymaster 5000\n")

    failures = []
    procs = []
    try:
        p, _ = launch([redispath, "--port", str(MASTER_PORT), "--save", ""], MASTER_PORT)
        procs.append(p)
        p, _ = launch([redispath, conf, "--sentinel"], RSENT_PORT)
        procs.append(p)
        p, fc = launch([binpath, "--port", str(FSENT_PORT), "--sentinel"], FSENT_PORT)
        procs.append(p)
        fc.cmd("SENTINEL", "MONITOR", "mymaster", "127.0.0.1", str(MASTER_PORT), "2")
        fc.cmd("SENTINEL", "SET", "mymaster", "down-after-milliseconds", "5000")
        time.sleep(0.3)
        rc = Conn(RSENT_PORT)

        # 1. Error / config wording byte-exact.
        for case in ERROR_CASES:
            o, f = rc.cmd(*case), fc.cmd(*case)
            if o != f:
                failures.append(f"{' '.join(case)}: redis={o!r} fr={f!r}")

        # 2. Per-master info: NO RESP3 map (`%`) may leak to a RESP2 client
        #    (SENTINEL MASTER is the map directly; MASTERS/SLAVES/SENTINELS
        #    nest it inside an outer array). Parse() returns a dict only for a
        #    `%` frame, so "contains a dict" == "a map appeared".
        for sub in (["SENTINEL", "MASTER", "mymaster"], ["SENTINEL", "MASTERS"]):
            if _contains_map(Conn(FSENT_PORT).cmd(*sub)):
                failures.append(f"{' '.join(sub)} RESP2 reply contains a map (%) — must downconvert")
            if _contains_map(Conn(RSENT_PORT).cmd(*sub)):
                failures.append(f"oracle {' '.join(sub)} RESP2 has a map — env issue")
        # Under RESP3 the per-master info IS a map.
        mt = reply_type(FSENT_PORT, ["SENTINEL", "MASTER", "mymaster"], hello3=True)
        if mt != b"%":
            failures.append(f"SENTINEL MASTER RESP3 reply type {mt!r}, want b'%'")
    finally:
        for p in reversed(procs):
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()
        try:
            os.remove(conf)
        except OSError:
            pass

    if failures:
        print("FAIL: SENTINEL surface divergences:")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    print(f"OK: SENTINEL error wording + RESP2/RESP3 reply types byte-exact vs "
          f"redis-sentinel 7.2.4 ({len(ERROR_CASES)} cases + 2x2 reply-type checks)")
    sys.exit(0)


if __name__ == "__main__":
    main()
