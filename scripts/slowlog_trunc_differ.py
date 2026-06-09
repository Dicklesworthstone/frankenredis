#!/usr/bin/env python3
"""slowlog_trunc_differ.py — SLOWLOG argv truncation parity vs redis 7.2.4.

Upstream slowlog.c::slowlogCreateEntry caps a logged command at
SLOWLOG_ENTRY_MAX_ARGC (32) arguments — the last retained slot becomes a
"... (N more arguments)" summary — and trims any single argument longer than
SLOWLOG_ENTRY_MAX_STRING (128) bytes to "<128 bytes>... (M more bytes)".
fr previously stored the full argv verbatim (bug frankenredis-ccpjj).

This gate sets slowlog-log-slower-than 0, runs commands that exercise both
caps, and asserts fr's SLOWLOG GET argv matches redis byte-for-byte.

Self-launches a clean fr + redis pair. Usage: [--bin FR] [--redis-bin REDIS]
"""
import argparse, os, socket, subprocess, sys, time, tempfile


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
        if t in (b"+", b"-"): return r
        if t == b":": return int(r)
        if t == b"$":
            n = int(r)
            if n < 0: return None
            while len(self.b) < n + 2: self.b += self.s.recv(65536)
            d, self.b = self.b[:n], self.b[n+2:]; return d
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.rd() for _ in range(n)]
        return line
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o); return self.rd()


def collect(port):
    c = Conn(port)
    c.cmd("CONFIG", "SET", "slowlog-log-slower-than", "0")
    c.cmd("SLOWLOG", "RESET")
    # >32 args (RPUSH key + 40 values = 42 args total)
    c.cmd("RPUSH", "biglist", *[b"v%d" % i for i in range(40)])
    # arg > 128 bytes
    c.cmd("SET", "longkey", b"A" * 200)
    # exactly 32 args (no summary), exactly-128B arg (no trim)
    c.cmd("MSET", *[b"k%d" % i for i in range(15) for _ in (0, 1)][:30])
    c.cmd("SET", "edge128", b"B" * 128)
    out = {}
    for e in c.cmd("SLOWLOG", "GET"):
        out[e[3][0]] = e[3]   # keyed by command name
    return out


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") == b"PONG": return True
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

    rdir = tempfile.mkdtemp(prefix="fr_slowlogtrunc_")
    fp, rp = free_port(), free_port()
    procs = []
    try:
        procs.append(subprocess.Popen([fr, "--port", str(fp), "--enable-debug-command", "yes"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen(
            [redis, "--port", str(rp), "--dir", rdir, "--save", "", "--appendonly", "no",
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        rf, ff = collect(rp), collect(fp)
    finally:
        for p in procs: p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()

    diffs = []
    for k in sorted(set(rf) | set(ff)):
        if rf.get(k) != ff.get(k):
            diffs.append((k, ff.get(k), rf.get(k)))
    for k, f, r in diffs:
        print(f"  [DIFF] {k.decode(errors='replace')}\n    fr={f!r}\n    rd={r!r}")
    if diffs:
        print(f"FAIL — {len(diffs)} slowlog argv divergence(s) vs redis 7.2.4")
        return 1
    print(f"PASS — SLOWLOG argv truncation parity vs redis 7.2.4 ({len(rf)} commands)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
