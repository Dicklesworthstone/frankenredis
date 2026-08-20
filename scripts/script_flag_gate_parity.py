"""Does the missing scriptFlagsToCmdFlags show up on the OOM gate too? (frankenredis-3bda1)

scriptFlagsToCmdFlags (script.c:111) grants CMD_DENYOOM unless the script declares
allow-oom or no-writes:

    if (!(script_flags & (SCRIPT_FLAG_ALLOW_OOM | SCRIPT_FLAG_NO_WRITES))) cmd_flags |= CMD_DENYOOM;

so over maxmemory a SHEBANG script or a FUNCTION with no such flag should be refused by the
ORDINARY command-level OOM gate, before the script body runs. A no-shebang EVAL keeps the
table's flags (evalGetCommandFlags returns early on EVAL_COMPAT_MODE), so it is NOT refused
at the command level -- its inner redis.call is refused instead. Those are different errors
and different moments, and the distinction is the whole point.

CONTROL: a plain SET must be refused with OOM on both engines before any row is read. If it
is not, maxmemory never took effect and every row below is vacuous. Exit 2 says that.

Verdicts, not timings; no timed run, so host load does not bear on the result.
"""
import os, shutil, socket, subprocess, sys, tempfile, time

REPO = "/data/projects/frankenredis"
FR = f"{REPO}/target/release/frankenredis"
REDIS = f"{REPO}/legacy_redis_code/redis/src/redis-server"

LIB = ("#!lua name=oomlib\n"
       "redis.register_function('oom_write', function(keys, args) "
       "return redis.call('set', keys[1], 'v') end)\n"
       "redis.register_function{function_name='oom_ro', "
       "callback=function(keys, args) return 1 end, flags={'no-writes'}}\n")


def enc(argv):
    out = b"*%d\r\n" % len(argv)
    for a in argv:
        b = a.encode() if isinstance(a, str) else a
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


class Conn:
    def __init__(self, port, timeout=10.0):
        self.s = socket.create_connection(("127.0.0.1", port), timeout)
        self.s.settimeout(timeout); self.buf = b""

    def _line(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d: raise EOFError
            self.buf += d
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line(); t, rest = line[:1], line[1:]
        if t in (b"+", b":"): return rest.decode("latin1")
        if t == b"-": return "ERR> " + rest.decode("latin1")
        if t == b"$":
            n = int(rest)
            if n == -1: return None
            while len(self.buf) < n + 2: self.buf += self.s.recv(65536)
            v, self.buf = self.buf[:n], self.buf[n + 2:]
            return v.decode("latin1")
        if t == b"*":
            n = int(rest)
            if n == -1: return None
            return [self._read() for _ in range(n)]
        return line.decode("latin1")

    def cmd(self, *argv):
        try:
            self.s.sendall(enc(list(argv)))
            return self._read()
        except (socket.timeout, EOFError, OSError) as e:
            return f"<NO REPLY {type(e).__name__}>"

    def close(self):
        try: self.s.close()
        except OSError: pass


def start(binary, port, workdir, name):
    os.makedirs(workdir, exist_ok=True)
    p = subprocess.Popen([binary, "--port", str(port), "--dir", workdir, "--save", ""],
                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, cwd=workdir)
    deadline = time.time() + 25
    while time.time() < deadline:
        if p.poll() is not None:
            print(f"HARNESS: {name} rc={p.returncode} "
                  f"{p.stderr.read().decode('utf-8','replace')[-400:]}")
            sys.exit(2)
        try:
            s = socket.create_connection(("127.0.0.1", port), 0.3); s.close(); return p
        except OSError:
            time.sleep(0.2)
    p.kill(); print(f"HARNESS: {name} never listened"); sys.exit(2)


ROWS = [
    ("CONTROL plain SET",      ["SET", "ctl:k", "v"]),
    ("CONTROL plain GET",      ["GET", "ctl:k"]),
    ("eval compat write",      ["EVAL", "return redis.call('set', KEYS[1], 'v')", "1", "e:k"]),
    ("eval compat readonly",   ["EVAL", "return 1", "0"]),
    ("eval shebang no flags",  ["EVAL", "#!lua\nreturn 1", "0"]),
    ("eval shebang allow-oom", ["EVAL", "#!lua flags=allow-oom\nreturn 1", "0"]),
    ("eval shebang no-writes", ["EVAL", "#!lua flags=no-writes\nreturn 1", "0"]),
    ("fcall no-flag fn",       ["FCALL", "oom_write", "1", "f:k"]),
    ("fcall_ro no-writes fn",  ["FCALL_RO", "oom_ro", "0"]),
]


def run(engine, binary, port, root):
    d = os.path.join(root, engine)
    shutil.rmtree(d, ignore_errors=True)
    proc = start(binary, port, d, engine)
    out = {}
    try:
        c = Conn(port)
        out["_load"] = str(c.cmd("FUNCTION", "LOAD", LIB))[:40]
        c.cmd("CONFIG", "SET", "maxmemory-policy", "noeviction")
        c.cmd("SET", "filler", "x" * 100000)
        c.cmd("CONFIG", "SET", "maxmemory", "1")
        for name, argv in ROWS:
            cc = Conn(port)
            out[name] = cc.cmd(*argv)
            cc.close()
        c.cmd("CONFIG", "SET", "maxmemory", "0")
        c.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=6)
        except subprocess.TimeoutExpired: proc.kill()
    return out


root = tempfile.mkdtemp(prefix="scriptoom_")
try:
    a = run("redis", REDIS, 29201, root)
    b = run("fr", FR, 29202, root)
finally:
    shutil.rmtree(root, ignore_errors=True)

print(f"FUNCTION LOAD  redis={a['_load']!r}  fr={b['_load']!r}\n")
is_oom = lambda r: isinstance(r, str) and "OOM" in r
if not (is_oom(a["CONTROL plain SET"]) and is_oom(b["CONTROL plain SET"])):
    print("INCONCLUSIVE: a plain SET was not OOM-refused, so maxmemory never took "
          "effect and every row below is vacuous.")
    print(f"   redis {a['CONTROL plain SET']!r}\n   fr    {b['CONTROL plain SET']!r}")
    sys.exit(2)

div = 0
for name, _ in ROWS:
    same = a[name] == b[name]
    cls = (is_oom(a[name]) == is_oom(b[name]))
    if not cls: div += 1
    tag = "AGREE  " if same else ("CLASS-OK" if cls else "DIVERGE")
    print(f"{tag} {name}")
    print(f"        redis  {str(a[name])[:88]!r}")
    print(f"        fr     {str(b[name])[:88]!r}")
print(f"\n{len(ROWS)-div}/{len(ROWS)} rows agree on OOM CLASS; {div} divergent")
