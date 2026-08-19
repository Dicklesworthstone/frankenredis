#!/usr/bin/env python3
"""Live same-invocation differential for the Lua PARSER's nesting bound (frankenredis-5h2lu).

That bead is a survival question, not a wording question: an unbounded recursive-descent parser
handed a deeply nested chunk exhausts the thread stack and ABORTS the process, which no in-process
test can observe -- an abort is not a catchable failure. The only honest instrument is a server that
either answers or stops answering, so every row here re-PINGs after the reply and reports whether the
engine is still alive.

Both engines get the SAME chunk at each depth, in one invocation, so the comparison cannot drift on
build, host or load. Three shapes, because upstream's `enterlevel` (lparser.c:276) sits on the
subexpression path and the constructor path separately, and a bound that covers one may miss the
other:

  parens       return ((((...1...))))     nested subexpressions
  tables       return {{{{...}}}}         nested constructors
  calls        return f(f(f(...f(1)...))) nested call arguments

The depth ladder brackets upstream's LUAI_MAXCCALLS (200, luaconf.h:468) and then jumps far past it,
because the interesting failure is not the boundary -- it is what a chunk 60x over the bound does.
"""
import os
import socket
import subprocess
import sys
import tempfile
import time

ROOT = "/data/projects/frankenredis"
FR = os.path.join(ROOT, "target/release/frankenredis")
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
FR_PORT, REDIS_PORT = 7791, 7792

SHAPES = {
    "parens": lambda n: "return " + "(" * n + "1" + ")" * n,
    "tables": lambda n: "return " + "{" * n + "}" * n,
    "calls":  lambda n: "local f = function(x) return x end return " + "f(" * n + "1" + ")" * n,
}
DEPTHS = [10, 150, 199, 200, 201, 250, 1000, 12000]


def loadavg():
    with open("/proc/loadavg") as f:
        return " ".join(f.read().split()[:3])


def mhz():
    try:
        with open("/proc/cpuinfo") as f:
            v = [float(l.split(":")[1]) for l in f if l.startswith("cpu MHz")]
        return "%.0f" % (sum(v) / len(v)) if v else "?"
    except OSError:
        return "?"


def start(binary, port, tag, env_extra=None):
    d = tempfile.mkdtemp(prefix=f"parsedepth_{tag}_")
    env = dict(os.environ)
    if env_extra:
        env.update(env_extra)
    p = subprocess.Popen([binary, "--port", str(port), "--dir", d, "--save", ""],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    for _ in range(120):
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return p
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"{tag} did not start on {port}")


def resp(*args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        b = a if isinstance(a, bytes) else str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


def call(port, *args, timeout=25.0):
    """(ok, text). ok=False means no reply -- the case a wording diff would silently miss."""
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout)
        s.settimeout(timeout)
        s.sendall(resp(*args))
        buf = b""
        while not buf.endswith(b"\r\n"):
            c = s.recv(65536)
            if not c:
                return (False, "CONNECTION CLOSED")
            buf += c
            if len(buf) > 2_000_000:
                break
        s.close()
        return (True, buf.decode(errors="replace").strip())
    except (OSError, socket.timeout) as e:
        return (False, "NO REPLY (%s)" % type(e).__name__)


def verdict(ok, txt):
    if not ok:
        return txt
    one = txt.replace("\r\n", " ")
    return ("ERR: " + one[1:64]) if one.startswith("-") else ("OK " + one[:40])


def self_test():
    """Prove the DIVERGE path fires, because a differential that reports parity it cannot detect is
    worse than no differential. Feeds the comparison one matching and one mismatching pair."""
    fr = start(FR, FR_PORT, "fr", {"FR_SHARED_NOTHING_PARTITIONS": "4"})
    try:
        a = call(FR_PORT, "EVAL", SHAPES["parens"](10), "0")
        b = call(FR_PORT, "EVAL", SHAPES["parens"](10), "0")
        c = call(FR_PORT, "EVAL", SHAPES["parens"](12000), "0")
        assert a == b, "identical scripts must compare equal: %r vs %r" % (a, b)
        assert a != c, "an accepted chunk and a refused one must NOT compare equal"
        # And the failure mode this script was rewritten to close, proved on the formatter directly
        # rather than through a live pair that happens to be byte-identical: two DIFFERENT replies
        # collapse to the same display string, so comparing verdict() reports a parity it never
        # checked. Every boundary row above truncates inside "user_script:1: chunk", so this is the
        # live shape, not a contrived one.
        long_a, long_b = "-" + "x" * 70 + "A", "-" + "x" * 70 + "B"
        assert long_a != long_b
        assert verdict(True, long_a) == verdict(True, long_b), \
            "verdict() must be shown to LOSE information, else this test guards nothing"
        print("SELF-TEST PASS: equality, inequality, and verdict() proven lossy")
        print("  full refusal text: %s" % c[1].replace("\r\n", " "))
    finally:
        fr.terminate()
        try:
            fr.wait(timeout=10)
        except subprocess.TimeoutExpired:
            fr.kill()
    return 0


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else ""
    if only == "--self-test":
        return self_test()
    for b in (FR, REDIS):
        if not os.path.exists(b):
            raise SystemExit("missing binary: " + b)
    fr = start(FR, FR_PORT, "fr", {"FR_SHARED_NOTHING_PARTITIONS": "4"})
    rd = start(REDIS, REDIS_PORT, "redis")
    bad = 0
    try:
        print("lua parser depth differential -- fr vs redis 7.2.4, one invocation")
        print("start loadavg %s, cpu MHz %s" % (loadavg(), mhz()))
        for shape, build in SHAPES.items():
            if only and only not in shape:
                continue
            print()
            print("%-7s %-8s %-52s %-52s" % ("shape", "depth", "fr", "redis 7.2.4"))
            print("-" * 122)
            for d in DEPTHS:
                script = build(d)
                f_ok, f_txt = call(FR_PORT, "EVAL", script, "0")
                r_ok, r_txt = call(REDIS_PORT, "EVAL", script, "0")
                # Compare the FULL reply, display the short form. Comparing what is PRINTED is how a
                # differential reports parity it never checked: every boundary row here truncates at
                # the same "user_script:1: chunk" prefix and would match on display alone.
                same = "" if (f_ok, f_txt) == (r_ok, r_txt) else "  <-- DIVERGE"
                if same:
                    bad += 1
                    print("        fr    full: %s" % f_txt.replace("\r\n", " ")[:200])
                    print("        redis full: %s" % r_txt.replace("\r\n", " ")[:200])
                print("%-7s %-8d %-52s %-52s%s"
                      % (shape, d, verdict(f_ok, f_txt), verdict(r_ok, r_txt), same))
                for port, name, ok in ((FR_PORT, "fr", f_ok), (REDIS_PORT, "redis", r_ok)):
                    if not ok:
                        print("        %s alive after this row? %s" % (name, call(port, "PING")[0]))
        print()
        print("fr alive at end %s, redis alive at end %s, end loadavg %s"
              % (call(FR_PORT, "PING")[0], call(REDIS_PORT, "PING")[0], loadavg()))
        print("%d diverging rows" % bad)
    finally:
        for p in (fr, rd):
            p.terminate()
        for p in (fr, rd):
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
