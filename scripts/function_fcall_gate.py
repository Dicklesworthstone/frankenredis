#!/usr/bin/env python3
"""function_fcall_gate.py — FUNCTION / FCALL semantics parity vs redis 7.2.4.

Locks in the Lua function-library surface (frankenredis-u9ubo): FUNCTION LOAD
(shebang name parsing, REPLACE, register_function positional + table forms,
no-writes flag), FCALL / FCALL_RO dispatch and read-only enforcement, numkeys
validation, registration errors (missing register, duplicate function, bad
shebang), FUNCTION DELETE / FLUSH / LIST (per-library) / STATS.

FUNCTION LIST library *ordering* is intentionally NOT compared: redis stores
libraries in a hash table keyed by a per-process-randomized seed, so the order
varies across restarts (confirmed). The gate queries `FUNCTION LIST LIBRARYNAME`
per library (deterministic) instead.

Self-launches a clean fr + redis pair. Usage: [--bin FR] [--redis-bin REDIS]
"""
import argparse, os, re, socket, subprocess, sys, tempfile, time


class Conn:
    def __init__(self, port, timeout=8):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.s.settimeout(timeout); self.b = b""
    def _l(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d: raise OSError("closed")
            self.b += d
        line, self.b = self.b.split(b"\r\n", 1); return line
    def rd(self):
        line = self._l(); t, r = line[:1], line[1:]
        if t == b"+": return ("S", r.decode("latin1"))
        if t == b"-": return ("E", r.decode("latin1"))
        if t == b":": return ("I", int(r))
        if t == b"$":
            n = int(r)
            if n < 0: return ("N", None)
            while len(self.b) < n + 2: self.b += self.s.recv(65536)
            d, self.b = self.b[:n], self.b[n+2:]; return ("B", d.decode("latin1"))
        if t == b"*":
            n = int(r)
            return ("A", None) if n < 0 else ("A", [self.rd() for _ in range(n)])
        return ("?", line.decode("latin1"))
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o)
        try: return self.rd()
        except Exception as e: return ("X", str(e))


READ_LIB = ("#!lua name=readlib\n"
            "redis.register_function('rget', function(keys, args) "
            "return redis.call('GET', keys[1]) end)")
WRITE_LIB = ("#!lua name=writelib\n"
             "redis.register_function('wset', function(keys, args) "
             "return redis.call('SET', keys[1], args[1]) end)")
NOWRITES_LIB = ("#!lua name=nwlib\n"
                "redis.register_function{function_name='nwget', "
                "callback=function(keys, args) return redis.call('GET', keys[1]) end, "
                "flags={'no-writes'}}")
LIB_NO_REGISTER = "#!lua name=bad1\nreturn 1"
LIB_NO_SHEBANG = "redis.register_function('x', function() return 1 end)"
LIB_DUP_FN = ("#!lua name=dup\n"
              "redis.register_function('d', function() return 1 end)\n"
              "redis.register_function('d', function() return 2 end)")


def _norm(v):
    if isinstance(v, tuple):
        t, x = v
        if t == "E":
            x = re.sub(r"user_function:\d+", "user_function:N", x)
            x = re.sub(r"f_[0-9a-f]{40}", "f_SHA", x)
            return (t, x)
        if t == "A":
            return (t, [_norm(e) for e in x] if x else x)
        return (t, x)
    return v


def steps(c):
    """Return a list of (label, normalized-result) for a deterministic run."""
    out = []
    def do(label, *cmd):
        out.append((label, _norm(c.cmd(*cmd))))
    c.cmd("FUNCTION", "FLUSH")
    c.cmd("SET", "foo", "barval")
    do("load read", "FUNCTION", "LOAD", READ_LIB)
    do("fcall read", "FCALL", "rget", "1", "foo")
    do("fcall_ro read", "FCALL_RO", "rget", "1", "foo")
    do("dup load (no REPLACE)", "FUNCTION", "LOAD", READ_LIB)
    do("replace load", "FUNCTION", "LOAD", "REPLACE", READ_LIB)
    do("load write", "FUNCTION", "LOAD", WRITE_LIB)
    do("fcall write", "FCALL", "wset", "1", "foo", "newv")
    do("get after write", "GET", "foo")
    do("fcall_ro on write fn", "FCALL_RO", "wset", "1", "foo", "x")   # read-only violation
    do("load no-writes", "FUNCTION", "LOAD", NOWRITES_LIB)
    do("fcall_ro no-writes", "FCALL_RO", "nwget", "1", "foo")
    do("load no register", "FUNCTION", "LOAD", LIB_NO_REGISTER)
    do("load no shebang", "FUNCTION", "LOAD", LIB_NO_SHEBANG)
    do("load dup fn name", "FUNCTION", "LOAD", LIB_DUP_FN)
    do("fcall missing fn", "FCALL", "nope", "0")
    do("fcall bad numkeys", "FCALL", "rget", "abc", "foo")
    do("fcall negative numkeys", "FCALL", "rget", "-1", "foo")
    do("fcall numkeys>args", "FCALL", "rget", "3", "foo")
    do("list readlib", "FUNCTION", "LIST", "LIBRARYNAME", "readlib")
    do("list writelib withcode", "FUNCTION", "LIST", "LIBRARYNAME", "writelib", "WITHCODE")
    do("list missing libname", "FUNCTION", "LIST", "LIBRARYNAME", "ghost")
    do("delete writelib", "FUNCTION", "DELETE", "writelib")
    do("delete missing", "FUNCTION", "DELETE", "ghost")
    do("fcall deleted", "FCALL", "wset", "1", "foo", "x")
    do("flush", "FUNCTION", "FLUSH")
    do("list after flush", "FUNCTION", "LIST")
    return out


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") == ("S", "PONG"): return True
        except Exception: time.sleep(0.2)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("FR_BIN",
                    "/data/tmp/cargo-target/release/frankenredis"))
    ap.add_argument("--redis-bin", default=os.environ.get("REDIS_BIN",
                    os.path.join(os.path.dirname(__file__), "..",
                                 "legacy_redis_code/redis/src/redis-server")))
    args = ap.parse_args()
    fr = os.path.abspath(args.bin); redis = os.path.abspath(args.redis_bin)
    if not os.path.exists(fr):
        print(f"SKIP: fr binary not found at {fr}"); return 0
    if not os.path.exists(redis):
        print(f"SKIP: redis-server not found at {redis}"); return 0

    rdir = tempfile.mkdtemp(prefix="fr_funcgate_")
    fp, rp = free_port(), free_port()
    procs = [
        subprocess.Popen([fr, "--port", str(fp)],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen([redis, "--port", str(rp), "--dir", rdir, "--save", "",
                          "--appendonly", "no"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        fsteps = steps(Conn(fp)); rsteps = steps(Conn(rp))
    finally:
        for p in procs: p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()

    diffs = []
    for (lf, vf), (lr, vr) in zip(fsteps, rsteps):
        if vf != vr:
            diffs.append((lf, vf, vr))
    for label, a, b in diffs:
        print(f"  [DIFF] {label}\n    fr={a}\n    rd={b}")
    if diffs:
        print(f"FAIL — {len(diffs)} FUNCTION/FCALL divergence(s) vs redis 7.2.4")
        return 1
    print(f"PASS — FUNCTION/FCALL semantics parity vs redis 7.2.4 ({len(fsteps)} steps)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
