#!/usr/bin/env python3
"""aof_propagation_stream_gate.py — AOF propagation-stream parity vs redis 7.2.4.

Verifies the exact command stream a master PROPAGATES (to AOF / replicas), not
just the converged final state. This catches "wrong-but-convergent" rewrites that
scripts/replication_convergence_gate.py cannot: e.g. propagating BLPOP verbatim
instead of the rewritten LPOP would still converge replica state, but breaks
chained replicas, MONITOR consumers, and AOF-inspection tooling.

Both impls run with appendonly enabled and appendfsync=always; we parse the incr
AOF (the literal propagated RESP command stream) and compare fr vs redis
byte-for-byte after masking absolute-ms timestamps (PXAT/PEXPIREAT wall-clock).

Only DETERMINISTIC write commands are exercised — no partial SPOP/SRANDMEMBER/
HRANDFIELD, whose member choice legitimately differs between independent servers.
The rewrites locked in: INCRBYFLOAT/HINCRBYFLOAT->SET/HSET, SPOP-all->DEL,
GETDEL->DEL, GETSET->SET, GETEX->PERSIST/PEXPIREAT/DEL, SETEX/SET EX->SET PXAT,
EXPIRE-family->PEXPIREAT, (B)LMPOP/(B)ZMPOP->LPOP/RPOP/ZPOPMIN/MAX,
BLMOVE/BRPOPLPUSH->LMOVE/RPOPLPUSH, delete-on-empty->DEL. (frankenredis-aqw97)

Self-launches a clean fr + redis pair. Usage: [--bin FR] [--redis-bin REDIS]
"""
import argparse, glob, os, re, socket, subprocess, sys, tempfile, time


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


# Deterministic write commands. Each propagated form is fully determined by argv
# + pre-state, so fr and redis must emit an identical stream.
TESTS = [
    ("RPUSH", "L", "a", "b", "c", "d", "e"),
    ("SADD", "S", "x", "y", "z", "1", "2", "3"),
    ("ZADD", "Z", "1", "a", "2", "b", "3", "c"),
    ("HSET", "H", "f1", "v1", "f2", "v2"),
    ("SET", "str", "hello"),
    ("MSET", "ca", "1", "cb", "2"),
    # --- rewrites (deterministic) ---
    ("LPOP", "L", "2"),                       # verbatim
    ("LMPOP", "2", "nope", "L", "LEFT", "COUNT", "1"),   # -> LPOP L 1
    ("ZMPOP", "2", "nope", "Z", "MIN"),       # -> ZPOPMIN Z 1
    ("INCRBYFLOAT", "ctr", "1.5"),            # -> SET ctr 1.5 KEEPTTL
    ("HINCRBYFLOAT", "H", "n", "2.5"),        # -> HSET H n 2.5
    ("GETSET", "str", "world"),               # -> SET str world
    ("GETDEL", "cb"),                         # -> DEL cb
    ("COPY", "ca", "cc"),                     # verbatim
    ("SETEX", "tk", "100", "tv"),             # -> SET tk tv PXAT <ms>
    ("GETEX", "tk", "PERSIST"),               # -> PERSIST tk
    ("GETEX", "ca", "EX", "200"),             # -> PEXPIREAT ca <ms>
    ("SET", "ex1", "v", "EX", "100"),         # -> SET ex1 v PXAT <ms>
    ("SET", "ex2", "v", "NX", "EX", "100"),   # -> SET ex2 v PXAT <ms> (NX dropped)
    ("SET", "ex3", "v", "EX", "100", "GET"),  # -> SET ex3 v PXAT <ms> (GET dropped)
    ("EXPIRE", "str", "1000"),                # -> PEXPIREAT str <ms>
    ("PEXPIRE", "str", "1000000"),            # -> PEXPIREAT str <ms>
    ("SETRANGE", "sr", "5", "xyz"),           # verbatim
    ("SETBIT", "bit", "7", "1"),              # verbatim
    ("ZADD", "Z", "GT", "CH", "5", "a"),      # verbatim
    ("LINSERT", "L", "BEFORE", "c", "NEW"),   # verbatim
    ("INCR", "ictr"),                         # verbatim
    ("APPEND", "str", "!!!"),                 # verbatim
    # --- delete-on-empty ---
    ("SADD", "S1", "a", "b", "c"),
    ("SPOP", "S1", "3"),                      # pop ALL -> DEL S1
    ("RPUSH", "L2", "a", "b", "c"),
    ("LMPOP", "1", "L2", "LEFT", "COUNT", "99"),   # pop all -> LPOP L2 3
    ("ZADD", "Z2", "1", "a"),
    ("ZPOPMIN", "Z2", "5"),                   # verbatim
    ("SADD", "SS", "a", "b", "c", "d"),
    ("SREM", "SS", "a", "b", "c", "d"),       # remove all -> key deleted
    ("HSET", "H1", "f", "v"),
    ("HDEL", "H1", "f"),                      # last field -> key deleted
]

_MS = re.compile(r"\b\d{12,14}\b")


def parse_aof(data):
    out = []; i = 0; n = len(data)
    while i < n:
        if data[i:i+1] != b"*":
            j = data.find(b"\r\n", i)
            if j < 0: break
            i = j + 2; continue
        j = data.find(b"\r\n", i); cnt = int(data[i+1:j]); i = j + 2
        args = []; ok = True
        for _ in range(cnt):
            if data[i:i+1] != b"$": ok = False; break
            j = data.find(b"\r\n", i); ln = int(data[i+1:j]); i = j + 2
            args.append(data[i:i+ln].decode("latin1")); i = i + ln + 2
        if ok and args:
            line = " ".join(args)
            if not line.upper().startswith(("SELECT", "MULTI", "EXEC")):
                out.append(_MS.sub("<ms>", line))
    return out


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") == b"PONG": return True
        except Exception: time.sleep(0.2)
    return False


def run(binpath, is_redis):
    d = tempfile.mkdtemp(prefix="fr_propstream_"); p = free_port()
    if is_redis:
        argv = [binpath, "--port", str(p), "--dir", d, "--save", "",
                "--appendonly", "yes", "--appendfsync", "always"]
    else:
        argv = [binpath, "--port", str(p), "--aof", os.path.join(d, "append.aof"),
                "--enable-debug-command", "yes"]
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not wait_up(p):
            return None
        c = Conn(p)
        if not is_redis:
            c.cmd("CONFIG", "SET", "appendfsync", "always")
        for t in TESTS:
            c.cmd(*t)
        time.sleep(0.6)
        files = (glob.glob(os.path.join(d, "appendonlydir", "*incr*.aof"))
                 + glob.glob(os.path.join(d, "*incr*.aof")))
        data = b""
        for f in sorted(files):
            data += open(f, "rb").read()
        return parse_aof(data)
    finally:
        proc.terminate()
        try: proc.wait(timeout=5)
        except Exception: proc.kill()


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

    ff = run(fr, False); rr = run(redis, True)
    if ff is None or rr is None:
        print("FAIL: a server did not start"); return 1

    diffs = []
    for i in range(max(len(ff), len(rr))):
        a = ff[i] if i < len(ff) else "<none>"
        b = rr[i] if i < len(rr) else "<none>"
        if a != b:
            diffs.append((i, a, b))
    for i, a, b in diffs:
        print(f"  [DIFF {i}]\n    fr={a!r}\n    rd={b!r}")
    if diffs:
        print(f"FAIL — {len(diffs)} propagated-command divergence(s) vs redis 7.2.4")
        return 1
    print(f"PASS — AOF propagation-stream parity vs redis 7.2.4 "
          f"({len(rr)} propagated commands)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
