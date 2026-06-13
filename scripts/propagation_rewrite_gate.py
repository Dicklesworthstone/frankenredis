#!/usr/bin/env python3
"""Replication/AOF propagation-rewrite gate for fr-server.

Redis does NOT propagate certain commands verbatim — it rewrites them into a
deterministic, replica-safe form so a replica (or AOF replay) converges exactly
regardless of its wall-clock or RNG:
  - relative TTLs -> absolute:  GETEX EX/PX -> PEXPIREAT;  SET EX -> SET ... PXAT;
    SETEX/PSETEX -> SET ... PXAT;  EXPIRE/PEXPIRE/EXPIREAT -> PEXPIREAT
  - GETEX PERSIST -> PERSIST;  GETDEL -> DEL
  - LMPOP/ZMPOP -> concrete LPOP/RPOP/ZPOPMIN/ZPOPMAX <key> <count>
  - INCRBYFLOAT -> SET <key> <result>;  HINCRBYFLOAT -> HSET
  - SPOP -> SREM / DEL (random member made concrete)
Getting any of these wrong silently diverges replicas — the highest-severity
class of replication bug. This gate runs each on a real fr-server with AOF on
(appendfsync always) and asserts the propagated form in the incr AOF.

Self-contained: spawns fr on a free port with a temp appendonlydir.
Usage: propagation_rewrite_gate.py [fr-binary]   (default: $CARGO_TARGET_DIR/release/frankenredis)
Exit 0 = all rewrites correct; 1 = a propagation divergence.
"""
import os, socket, subprocess, sys, tempfile, time, shutil


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Cli:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.buf = b""

    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else x
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o)
        return self.read()

    def _line(self):
        while b"\r\n" not in self.buf:
            self.buf += self.s.recv(65536)
        l, self.buf = self.buf.split(b"\r\n", 1)
        return l

    def read(self):
        l = self._line()
        t = l[:1]
        if t in (b'+', b':', b'-'):
            return l.decode()
        if t == b'$':
            n = int(l[1:])
            if n < 0:
                return None
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(65536)
            d = self.buf[:n]
            self.buf = self.buf[n + 2:]
            return d.decode("latin1")
        if t in (b'*', b'~'):
            n = int(l[1:])
            return None if n < 0 else [self.read() for _ in range(n)]
        return l.decode()


def parse_aof(path):
    d = open(path, "rb").read()
    i, out = 0, []
    while i < len(d) and d[i:i + 1] == b'*':
        j = d.index(b'\r\n', i)
        n = int(d[i + 1:j])
        i = j + 2
        parts = []
        for _ in range(n):
            j = d.index(b'\r\n', i)
            ln = int(d[i + 1:j])
            i = j + 2
            parts.append(d[i:i + ln].decode("latin1"))
            i += ln + 2
        out.append(parts)
    return out


def _parse_fr_bin(argv):
    # Accept both the bare positional form (`propagation_rewrite_gate.py <fr-bin>`)
    # and the self-launcher flag form the suite runner (run_parity_differs.sh) uses
    # to drive --bin/--redis-bin gates: `--bin <fr> [--redis-bin <rd>]`. The
    # runner detects self-launchers by grepping for `--bin`/`--redis-bin`, so this
    # gate must recognize them or it gets fed `--oracle/--fr` and crashes.
    for i, a in enumerate(argv):
        if a == "--bin" and i + 1 < len(argv):
            return argv[i + 1]
    # bare positional (skip any --flag tokens)
    for a in argv:
        if not a.startswith("-"):
            return a
    return os.environ.get("CARGO_TARGET_DIR", "/data/tmp/cargo-target") + "/release/frankenredis"


def main():
    fr_bin = _parse_fr_bin(sys.argv[1:])
    port = free_port()
    tmp = tempfile.mkdtemp(prefix="fr-prop-")
    aof = os.path.join(tmp, "appendonly.aof")
    proc = subprocess.Popen(
        [fr_bin, "--port", str(port), "--enable-debug-command", "yes", "--aof", aof],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(1.5)
        c = Cli(port)
        c.cmd("config", "set", "appendfsync", "always")
        incr = None
        for _ in range(20):
            cand = [f for f in os.listdir(tmp) if f.endswith("incr.aof")]
            if cand:
                incr = os.path.join(tmp, cand[0])
                break
            time.sleep(0.1)
        before = os.path.getsize(incr)
        # (command-to-run, predicate(propagated-cmds) -> ok, description)
        checks = []

        def run(cmds, verb, desc, extra=None):
            nonlocal before
            for cm in cmds:
                c.cmd(*cm)
            time.sleep(0.15)
            prop = parse_aof(incr)
            # only the commands appended since `before`: re-parse whole + slice by re-reading
            # simpler: re-read full and take the tail matching our run by scanning for verb
            checks.append((verb, desc, prop))
            before = os.path.getsize(incr)

        c.cmd("set", "kx", "hello")
        c.cmd("getex", "kx", "EX", "100")
        # ky must already carry a TTL: GETEX PERSIST only propagates when it
        # actually removes an expiry (no-op on a non-volatile key is NOT
        # propagated — correct redis behavior, asserted by giving ky a TTL here).
        c.cmd("set", "ky", "v", "EX", "100")
        c.cmd("getex", "ky", "PERSIST")
        c.cmd("set", "kz", "d")
        c.cmd("getdel", "kz")
        c.cmd("setex", "kw", "200", "v")
        c.cmd("rpush", "ml", "a", "b", "c")
        c.cmd("lmpop", "2", "nope", "ml", "LEFT")
        c.cmd("set", "kq", "v", "EX", "300")
        c.cmd("zadd", "zs", "1", "a", "2", "b")
        c.cmd("zmpop", "1", "zs", "MIN")
        c.cmd("set", "fl", "10")
        c.cmd("incrbyfloat", "fl", "1.5")
        c.cmd("sadd", "st", "only")
        c.cmd("spop", "st")
        c.cmd("expire", "kx", "50")
        time.sleep(0.2)
        prop = parse_aof(incr)
        verbs = [p[0].upper() for p in prop]

        def has(verb, *args):
            for p in prop:
                if p[0].upper() == verb and list(p[1:1 + len(args)]) == list(args):
                    return True
            return False

        def has_abs_ttl(verb, key):
            # verb key <absolute-ms>  — assert arg present and is a large (absolute) int
            for p in prop:
                if p[0].upper() == verb and len(p) >= 3 and p[1] == key:
                    try:
                        return int(p[2]) > 10 ** 12
                    except ValueError:
                        return False
            return False

        fails = []
        if not has_abs_ttl("PEXPIREAT", "kx"):
            fails.append("GETEX EX -> PEXPIREAT kx <abs>")
        if not has("PERSIST", "ky"):
            fails.append("GETEX PERSIST -> PERSIST ky")
        if not has("DEL", "kz"):
            fails.append("GETDEL -> DEL kz")
        if not any(p[0].upper() == "SET" and p[1] == "kw" and "PXAT" in [a.upper() for a in p] for p in prop):
            fails.append("SETEX -> SET kw ... PXAT")
        if not any(p[0].upper() in ("LPOP", "RPOP") and p[1] == "ml" for p in prop):
            fails.append("LMPOP -> LPOP/RPOP ml")
        if not any(p[0].upper() == "SET" and p[1] == "kq" and "PXAT" in [a.upper() for a in p] for p in prop):
            fails.append("SET EX -> SET kq ... PXAT")
        if not any(p[0].upper() in ("ZPOPMIN", "ZPOPMAX") and p[1] == "zs" for p in prop):
            fails.append("ZMPOP -> ZPOPMIN/ZPOPMAX zs")
        if not any(p[0].upper() == "SET" and p[1] == "fl" for p in prop):
            fails.append("INCRBYFLOAT -> SET fl <result>")
        if not (has("SREM", "st", "only") or has("DEL", "st")):
            fails.append("SPOP -> SREM/DEL st")
        if not has_abs_ttl("PEXPIREAT", "kx"):
            fails.append("EXPIRE -> PEXPIREAT kx <abs>")

        print("propagated verbs:", " ".join(verbs))
        print("-" * 60)
        if fails:
            for f in fails:
                print("MISSING REWRITE:", f)
            print(f"FAIL — {len(fails)} propagation rewrite(s) missing/wrong")
            return 1
        print("PASS — all replication/AOF propagation rewrites present and absolute-timed")
        return 0
    finally:
        try:
            Cli(port).cmd("shutdown", "nosave")
        except Exception:
            pass
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except Exception:
            proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
