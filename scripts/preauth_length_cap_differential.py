#!/usr/bin/env python3
"""Live differential for upstream's PRE-AUTH protocol length caps (frankenredis-2ubu0).

Upstream `processMultibulkBuffer` (networking.c) caps an UNAUTHENTICATED client twice:

    multibulk count   } else if (ll > 10 && authRequired(c)) {
                          addReplyError(c, "Protocol error: unauthenticated multibulk length");
    bulk length       } else if (ll > 16384 && authRequired(c)) {
                          addReplyError(c, "Protocol error: unauthenticated bulk length");

This is the defence against pre-auth memory exhaustion: without it a client that has not proved it
may talk to the server at all can make it allocate an arbitrarily large query buffer.

FOUR THINGS HAVE TO HOLD, and a probe that checks only the first is worth little:

  1. Over the cap while unauthenticated  -> the UNAUTHENTICATED wording, connection closed.
  2. AT the cap while unauthenticated    -> accepted. Upstream's test is `>`, so 10 and 16384 are
                                            legal; an off-by-one here rejects valid traffic.
  3. MALFORMED while unauthenticated     -> the ORDINARY "invalid ..." wording. Both upstream checks
                                            sit AFTER the validity checks, so a bad length must not
                                            acquire the unauthenticated wording.
  4. Over the cap AFTER AUTH             -> accepted. This is the half most likely to regress
                                            silently: the caps are chosen per parser-config call and
                                            fr CACHES that config per batch, so a client that
                                            authenticates must stop being capped.

Every case is compared byte-for-byte against vendored 7.2.4 in the same invocation.
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
FR_PORT, REDIS_PORT = 7871, 7872
PASSWORD = "s3cret"


def loadavg():
    with open("/proc/loadavg") as f:
        return " ".join(f.read().split()[:3])


def _cpu():
    with open("/proc/stat") as f:
        v = [int(x) for x in f.readline().split()[1:]]
    return sum(v), v[4]


def conditions(since):
    total_b, io_b = _cpu()
    d = total_b - since[0]
    iowait = (io_b - since[1]) / d * 100 if d > 0 else 0.0
    try:
        with open("/proc/cpuinfo") as f:
            s = [float(l.split(":")[1]) for l in f if l.startswith("cpu MHz")]
        mhz = "%.0f" % (sum(s) / len(s)) if s else "?"
    except OSError:
        mhz = "?"
    return "loadavg %s | iowait %.2f%% over the run | CPU %s MHz" % (loadavg(), iowait, mhz)


def start(binary, port, tag):
    d = tempfile.mkdtemp(prefix="preauth_%s_" % tag)
    p = subprocess.Popen(
        [binary, "--port", str(port), "--dir", d, "--save", "", "--requirepass", PASSWORD],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(150):
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return p
        except OSError:
            time.sleep(0.1)
    raise SystemExit("%s did not start on %d" % (tag, port))


def raw(port, payload: bytes, auth_first: bool = False, timeout=5.0):
    """Send raw bytes; return (reply_text, closed_by_server)."""
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout)
        s.settimeout(timeout)
        if auth_first:
            s.sendall(b"*2\r\n$4\r\nAUTH\r\n$%d\r\n%s\r\n" % (len(PASSWORD), PASSWORD.encode()))
            s.recv(4096)
        s.sendall(payload)
        buf = b""
        closed = False
        try:
            while len(buf) < 4096:
                chunk = s.recv(4096)
                if not chunk:
                    closed = True
                    break
                buf += chunk
                if buf.endswith(b"\r\n") and buf.count(b"\r\n") >= 1:
                    # give the server a moment to close if it intends to
                    s.settimeout(0.4)
                    try:
                        more = s.recv(4096)
                        if not more:
                            closed = True
                        else:
                            buf += more
                    except (socket.timeout, OSError):
                        pass
                    break
        except (socket.timeout, OSError):
            pass
        s.close()
        return buf.decode(errors="replace").replace("\r\n", " ").strip(), closed
    except OSError as e:
        return "<connect failed: %s>" % type(e).__name__, True


BIG = b"x" * 20000
CASES = [
    # (name, payload, auth_first)
    ("unauth_mbulk_11",     b"*11\r\n", False),
    ("unauth_mbulk_10",     b"*10\r\n$4\r\nPING\r\n", False),
    ("unauth_bulk_16385",   b"*1\r\n$16385\r\n", False),
    ("unauth_bulk_16384",   b"*1\r\n$16384\r\n", False),
    ("unauth_mbulk_bad",    b"*abc\r\n", False),
    ("unauth_mbulk_huge",   b"*99999999999\r\n", False),
    ("unauth_bulk_bad",     b"*1\r\n$abc\r\n", False),
    ("authed_mbulk_11",
     b"*11\r\n$4\r\nMSET\r\n$2\r\nk1\r\n$2\r\nv1\r\n$2\r\nk2\r\n$2\r\nv2\r\n"
     b"$2\r\nk3\r\n$2\r\nv3\r\n$2\r\nk4\r\n$2\r\nv4\r\n$2\r\nk5\r\n$2\r\nv5\r\n", True),
    ("authed_bulk_20000",
     b"*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$20000\r\n" + BIG + b"\r\n", True),
]


def main():
    for b in (FR, REDIS):
        if not os.path.exists(b):
            raise SystemExit("missing binary: " + b)
    since = _cpu()
    fr = start(FR, FR_PORT, "fr")
    rd = start(REDIS, REDIS_PORT, "redis")
    bad = 0
    try:
        print("pre-auth length caps -- fr vs redis 7.2.4, one invocation, requirepass set")
        print("%-20s %-42s %-42s" % ("case", "fr", "redis 7.2.4"))
        print("-" * 108)
        for name, payload, auth in CASES:
            a_txt, a_closed = raw(FR_PORT, payload, auth)
            b_txt, b_closed = raw(REDIS_PORT, payload, auth)
            a = "%s%s" % (a_txt[:34], "  [closed]" if a_closed else "")
            b = "%s%s" % (b_txt[:34], "  [closed]" if b_closed else "")
            same = (a_txt, a_closed) == (b_txt, b_closed)
            if not same:
                bad += 1
            print("%-20s %-42s %-42s%s" % (name, a, b, "" if same else "  <-- DIVERGE"))
            if not same:
                print("      fr    %r closed=%s" % (a_txt[:120], a_closed))
                print("      redis %r closed=%s" % (b_txt[:120], b_closed))
        print()
        print("%d diverging cases | %s" % (bad, conditions(since)))
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
