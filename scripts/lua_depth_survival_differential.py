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
        # (frankenredis-thread-stack-size-1tlyh) READ THE WHOLE REPLY, not the first line. The
        # reply-walk refusal is substituted AT THE ELEMENT'S POSITION -- upstream appends the error
        # where the nesting ran out and pops just that element -- so on a deep table it arrives
        # thousands of levels inside a nested array, behind a wall of `*1\r\n`. A reader that stops
        # at the first CRLF calls that "OK" and silently turns an unproven row into a passing one.
        buf = b""
        s.settimeout(timeout)
        try:
            while len(buf) < 8_000_000:
                chunk = s.recv(1 << 16)
                if not chunk:
                    break
                buf += chunk
                if buf.startswith(b"-") and buf.endswith(b"\r\n"):
                    break          # a top-level error IS one line
                s.settimeout(1.0)  # drain the rest quickly once flowing
        except socket.timeout:
            pass
        s.close()
        if not buf:
            return (False, "CONNECTION CLOSED (server died or dropped it)")
        depth_seen = buf.count(b"*1\r\n")
        if b"reached lua stack limit" in buf:
            return (True, "NESTED ERR reached lua stack limit at depth ~%d" % depth_seen)
        if buf.startswith(b"-"):
            return (True, "ERR " + buf[1:90].decode(errors="replace").replace("\r\n", " ").strip())
        return (True, "OK, nesting delivered ~%d" % depth_seen)
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
    # Plain Lua self-recursion. Not a serialiser walk -- this probes the INTERPRETER's own call
    # depth, which fr bounds with MAX_CALL_DEPTH while Lua bounds it by stack.
    "recurse": ("local function f(n) if n == 0 then return 0 end return 1 + f(n-1) end "
                "return f(%d)"),
    # Pattern captures. Not a stack question at all -- LUA_MAXCAPTURES is a fixed array in the
    # matcher -- but the same shape: a constant that has to fire at the same COUNT as upstream.
    "captures": ("local s = string.rep('x', %d) local p = string.rep('(x)', %d) "
                 "return string.match(s, p)"),
}


def bisect_ceiling(port, tag, tmpl, n_slots, cap):
    """Binary-search the deepest depth that still COMPLETES on one engine.

    (frankenredis-lua-call-depth-ug22x) A ladder of hand-picked depths reports which rungs
    pass; it cannot report the BOUNDARY, and the boundary is the number the residual gap is
    computed from. The doc comment on MAX_CALL_DEPTH quoted upstream as running "to ~16000
    and refusing by 18000" from such a ladder; bisection puts the exact ceiling at 19996,
    which moves the stated gap from ~21x to 26.1x.

    Liveness is checked after every FAILING probe, because the failure this whole tool exists
    to distinguish -- a guard firing versus the process aborting on a real stack overflow --
    looks identical from a single reply. A dead server ends the search rather than being
    bisected further, and says so.
    """
    def completes(n):
        # `call` returns ok=True for an ERROR REPLY on purpose -- its own contract is
        # "ok=False means the connection died", because process death is what the ladder
        # exists to catch. Bisection asks a DIFFERENT question, so it must not reuse that
        # flag: a refusal at depth n is exactly what bounds the search. Reading `ok` here
        # reported "fr reaches 1.0000x the incumbent's depth" against a stack-overflow
        # reply printed on the same line.
        ok, txt = call(port, "EVAL", tmpl % ((n,) * n_slots), "0")
        if not ok:
            if not alive(port):
                print("    %s DIED at depth %d -- process abort, not a guard: %s" % (tag, n, txt))
                return None, txt
            return False, txt
        if txt.startswith("ERR ") or txt.startswith("NESTED ERR"):
            return False, txt
        return True, txt

    ok, txt = completes(cap)
    if ok is None:
        return None, txt
    if ok:
        print("    %s: still completes at the cap %d; the ceiling is above it" % (tag, cap))
        return cap, txt
    lo, hi, detail = 1, cap, txt
    while lo + 1 < hi:
        mid = (lo + hi) // 2
        good, t = completes(mid)
        if good is None:
            return None, t
        if good:
            lo = mid
        else:
            hi, detail = mid, t
    return lo, detail


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
        # (frankenredis-thread-stack-size-1tlyh) Depths are overridable because the four walks
        # have DIFFERENT limits: encode/decode turn over at 1000, the reply walk at 2000, and
        # cmsgpack degrades at 16. A ladder that stops at 2000 exercises the reply walk without
        # ever reaching its bound, which is exactly the gap this argument closes.
        if len(sys.argv) > 2 and sys.argv[2] == "--bisect":
            cap = int(sys.argv[3]) if len(sys.argv) > 3 else 40000
            print("mode: BISECT -- exact ceiling per engine, cap %d, loadavg %s" % (cap, loadavg()))
            fr_max, fr_txt = bisect_ceiling(FR_PORT, "fr", tmpl, n_slots, cap)
            rd_max, rd_txt = bisect_ceiling(REDIS_PORT, "redis", tmpl, n_slots, cap)
            print()
            print("fr    deepest completing: %s" % fr_max)
            print("      first failure:      %s" % fr_txt)
            print("redis deepest completing: %s" % rd_max)
            print("      first failure:      %s" % rd_txt)
            if fr_max and rd_max:
                print()
                print("fr reaches %.4fx the incumbent's depth (%s vs %s), a %.1fx shortfall"
                      % (fr_max / rd_max, fr_max, rd_max, rd_max / fr_max))
            print("fr alive at end:    %s" % alive(FR_PORT))
            print("redis alive at end: %s" % alive(REDIS_PORT))
            print("loadavg at end: %s" % loadavg())
            return 0
        depths = ([int(x) for x in sys.argv[2].split(",")] if len(sys.argv) > 2
                  else [100, 900, 999, 1000, 1001, 2000])
        for depth in depths:
            la = loadavg()
            body = tmpl % ((depth,) * n_slots)
            f_ok, f_txt = call(FR_PORT, "EVAL", body, "0")
            r_ok, r_txt = call(REDIS_PORT, "EVAL", body, "0")
            def brief(ok, txt):
                # `call` already classifies; print its verdict rather than re-summarising it by
                # length, which is how a divergence hid behind two equal-looking "OK reply" cells.
                return txt if ok else txt
            print("%-8d %-52s %-52s   load %s" % (depth, brief(f_ok, f_txt), brief(r_ok, r_txt), la))
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
