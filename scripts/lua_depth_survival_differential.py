#!/usr/bin/env python3
"""Live same-invocation differential: does a cjson.encode INSIDE upstream's own depth limit survive?

frankenredis-thread-stack-size-1tlyh said fr sets no thread stack_size, so every spawned thread took
Rust's 2 MiB default -- and `run_shared_nothing_worker` serves connections on exactly such a thread.
The cjson encode bound landed at upstream's 1000 (frankenredis-cjson-encode-depth-zo5ac), but a limit
is only a defence if the engine survives everything it ADMITS: a 1000-deep encode measured >2 MiB of
frames in libtest, so fr could refuse nothing and still die. `c63b56ef1` sized the worker stacks at
8 MiB to fix it, UNBUILT and unverified. This verifies it against the incumbent, in one invocation.

Both engines get the SAME script at each depth. The question per row is not a number but a verdict:
does the server answer at all, and does it answer the same way.

Deliberately no shutdown command is sent to anything but the two servers this script starts itself,
each on its own port with its own dir.
"""
import os
import socket
import subprocess
import sys
import tempfile
import time

ROOT = "/data/projects/frankenredis"
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
FR = os.path.join(ROOT, "target/release/frankenredis")
FR_PORT, REDIS_PORT = 7731, 7732


def loadavg():
    with open("/proc/loadavg") as f:
        return " ".join(f.read().split()[:3])


def start(binary, port, tag, env_extra=None):
    d = tempfile.mkdtemp(prefix=f"depthdiff_{tag}_")
    env = dict(os.environ)
    if env_extra:
        env.update(env_extra)
    p = subprocess.Popen([binary, "--port", str(port), "--dir", d, "--save", ""],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", port), 0.2)
            s.close()
            return p
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"{tag} did not come up on {port}")


def resp(*args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        b = a if isinstance(a, bytes) else str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


def call(port, *args, timeout=20.0):
    """Returns (ok, text). ok=False means the connection died -- which is the interesting case."""
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout)
        s.settimeout(timeout)
        s.sendall(resp(*args))
        buf = b""
        while not buf.endswith(b"\r\n"):
            chunk = s.recv(65536)
            if not chunk:
                return (False, "CONNECTION CLOSED (server died or dropped it)")
            buf += chunk
            if len(buf) > 4_000_000:
                break
        s.close()
        return (True, buf[:120].decode(errors="replace").replace("\r\n", " ").strip())
    except (OSError, socket.timeout) as e:
        return (False, f"NO REPLY ({type(e).__name__})")


def alive(port):
    ok, _ = call(port, "PING", timeout=5.0)
    return ok


# (frankenredis-thread-stack-size-1tlyh) FOUR recursive walks over user-controlled structure, not
# one. The encode bound was verified first; the other three are bounded in FRAMES with no byte
# budget behind the number, or not bounded at all, so each needs its own survival row.
#
#   encode      cjson.encode over a table built at RUNTIME -- no syntax-level counter sees it
#   decode      cjson.decode over a STRING, the cheapest of the four to send
#   reply       the Lua -> RESP walk, bounded at 2000 frames by 4a438ed13
#   msgpack     cmsgpack.pack, bounded at 16 by upstream's own MAX_NESTING
WALKS = {
    "encode":  "local t = {} for i = 1, %d do t = {t} end return cjson.encode(t)",
    "decode":  "return cjson.decode(string.rep('[', %d) .. string.rep(']', %d))",
    "reply":   "local t = {} for i = 1, %d do t = {t} end return t",
    "msgpack": "local t = {} for i = 1, %d do t = {t} end return cmsgpack.pack(t)",
}

def main():
    for b in (REDIS, FR):
        if not os.path.exists(b):
            raise SystemExit(f"missing binary: {b}")
    fr = start(FR, FR_PORT, "fr", {"FR_SHARED_NOTHING_PARTITIONS": "4"})
    rd = start(REDIS, REDIS_PORT, "redis")
    print("live differential: cjson.encode at increasing nesting depth")
    print("fr   = %s" % FR)
    print("redis= %s" % REDIS)
    print()
    print("%-8s %-46s %-46s" % ("depth", "fr", "redis 7.2.4"))
    print("-" * 104)
    try:
        walk = sys.argv[1] if len(sys.argv) > 1 else "encode"
        if walk not in WALKS:
            raise SystemExit("walk must be one of: %s" % ", ".join(sorted(WALKS)))
        tmpl = WALKS[walk]
        n_slots = tmpl.count("%d")
        print("walk: %s" % walk)
        for depth in (100, 900, 999, 1000, 1001, 2000):
            la = loadavg()
            body = tmpl % ((depth,) * n_slots)
            f_ok, f_txt = call(FR_PORT, "EVAL", body, "0")
            r_ok, r_txt = call(REDIS_PORT, "EVAL", body, "0")
            def brief(ok, txt):
                if not ok:
                    return txt
                if txt.startswith("-"):
                    return "ERR: " + txt[1:70]
                return "OK reply (%d chars shown)" % len(txt)
            print("%-8d %-46s %-46s   load %s" % (depth, brief(f_ok, f_txt), brief(r_ok, r_txt), la))
            if not f_ok:
                print("         fr still alive after this row? %s" % alive(FR_PORT))
            if not r_ok:
                print("         redis still alive after this row? %s" % alive(REDIS_PORT))
        print()
        print("fr alive at end:    %s" % alive(FR_PORT))
        print("redis alive at end: %s" % alive(REDIS_PORT))
    finally:
        for p in (fr, rd):
            p.terminate()
        for p in (fr, rd):
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()


if __name__ == "__main__":
    sys.exit(main())
