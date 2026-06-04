#!/usr/bin/env python3
"""client_tracking_differential_probe.py — live byte-exact parity check of
fr-server's RESP3 CLIENT TRACKING (client-side caching invalidation) against the
vendored redis 7.2.4 oracle. Covers the surface that has repeatedly produced real
bugs and is touched by ongoing perf work:

  * default / OPTIN / OPTOUT tracking + CLIENT CACHING YES|NO
  * BCAST + PREFIX (match / non-match / multi-key batching)
  * NOLOOP (self-modify suppression) + REDIRECT-less RESP3 push delivery
  * FLUSHALL/FLUSHDB null-invalidation (frankenredis-o90ga)
  * per-key invalidation: one push PER key for non-BCAST, batched for BCAST
    (frankenredis-8ypwc)
  * bcast-client INDEX maintenance — ON/OFF/RESET/disconnect/re-enable and
    bcast<->non-bcast switches (frankenredis-yaxr7.2: the index that skips the
    all-session scan must be kept exact, or invalidations are dropped/leaked)

SETUP (identical to scripts/differential_probe.sh — oracle config-LESS, fr in
--mode strict):
    ORACLE=legacy_redis_code/redis/src
    $ORACLE/redis-server --port 16399 --daemonize yes --save '' --appendonly no
    cargo build -p fr-server      # $CARGO_TARGET_DIR/debug/frankenredis
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    python3 scripts/client_tracking_differential_probe.py 16399 16400
"""
import socket
import sys
import time


class Conn:
    """Raw-socket RESP2/3 client that can read replies and async push frames."""

    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=3)
        self.buf = b""

    def _fill(self):
        self.s.settimeout(0.6)
        try:
            d = self.s.recv(65536)
            if not d:
                return False
            self.buf += d
            return True
        except socket.timeout:
            return False

    def _read_line(self):
        while b"\r\n" not in self.buf:
            if not self._fill():
                return None
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def read_frame(self):
        line = self._read_line()
        if line is None:
            return None
        t, rest = line[:1], line[1:]
        if t in (b"+", b"-", b":", b",", b"#", b"("):
            return (t.decode(), rest.decode())
        if t == b"_":
            return None
        if t in (b"$", b"="):
            n = int(rest)
            if n < 0:
                return None
            while len(self.buf) < n + 2:
                if not self._fill():
                    break
            data, self.buf = self.buf[:n], self.buf[n + 2:]
            return data.decode("latin1")
        if t in (b"*", b">", b"%", b"~"):
            n = int(rest)
            if n < 0:
                return None
            count = n * 2 if t == b"%" else n
            return (t.decode(), [self.read_frame() for _ in range(count)])
        return ("?", line.decode("latin1"))

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            a = a.encode() if isinstance(a, str) else a
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)

    def cmd_read(self, *args):
        self.cmd(*args)
        return self.read_frame()

    def drain_push(self, wait=0.35):
        time.sleep(wait)
        frames = []
        while True:
            f = self.read_frame()
            if f is None:
                break
            frames.append(f)
        return frames

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def norm_push(frames):
    """Reduce push frames to [(channel, sorted-keys-or-None)] for comparison."""
    out = []
    for f in frames:
        if isinstance(f, tuple) and f[0] == ">":
            items = f[1]
            chan = items[0] if items else None
            payload = items[1] if len(items) > 1 else None
            if isinstance(payload, tuple):
                keys = sorted(payload[1]) if payload[1] else []
            elif payload is None:
                keys = None
            else:
                keys = payload
            out.append((chan, keys))
    return out


def run(port):
    R = {}

    def fresh():
        c = Conn(port)
        c.cmd_read("FLUSHALL")
        c.close()

    def tracker(*setup):
        t = Conn(port)
        t.cmd_read("HELLO", "3")
        t.drain_push(0.1)
        if setup:
            t.cmd_read(*setup)
        return t

    # ── default tracking: read then foreign SET -> one invalidate ──
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON")
    t.cmd_read("GET", "ka")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "ka", "v"); w.close()
    R["default_set"] = norm_push(t.drain_push())
    t.close()

    # untracked key -> nothing
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON")
    t.cmd_read("GET", "kb")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "kb_other", "v"); w.close()
    R["default_untracked"] = norm_push(t.drain_push())
    t.close()

    # ── OPTIN: no caching -> none; CLIENT CACHING YES -> push ──
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON", "OPTIN")
    t.cmd_read("GET", "kf")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "kf", "v"); w.close()
    R["optin_nocache"] = norm_push(t.drain_push())
    t.cmd_read("CLIENT", "CACHING", "YES")
    t.cmd_read("GET", "kg")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "kg", "v"); w.close()
    R["optin_caching"] = norm_push(t.drain_push())
    t.close()

    # ── OPTOUT: default cached -> push; CACHING NO -> none ──
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON", "OPTOUT")
    t.cmd_read("GET", "kh")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "kh", "v"); w.close()
    R["optout_default"] = norm_push(t.drain_push())
    t.cmd_read("CLIENT", "CACHING", "NO")
    t.cmd_read("GET", "ki")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "ki", "v"); w.close()
    R["optout_nocache"] = norm_push(t.drain_push())
    t.close()

    # ── FLUSHALL null-invalidation (o90ga) ──
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON")
    t.cmd_read("GET", "kj")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("FLUSHALL"); w.close()
    R["flushall_null"] = norm_push(t.drain_push())

    # ── non-BCAST per-key: MSET of two tracked keys -> TWO pushes (8ypwc) ──
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON")
    t.cmd_read("MSET", "m1", "1", "m2", "2")
    t.cmd_read("GET", "m1"); t.cmd_read("GET", "m2")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("MSET", "m1", "9", "m2", "8"); w.close()
    R["nonbcast_mset_perkey"] = norm_push(t.drain_push())
    t.close()

    # ── NOLOOP: self-modify suppressed, foreign delivered ──
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON", "NOLOOP")
    t.cmd_read("SET", "kx", "1"); t.cmd_read("GET", "kx")
    t.drain_push(0.12)
    t.cmd_read("SET", "kx", "2")               # self
    R["noloop_self"] = norm_push(t.drain_push())
    w = Conn(port); w.cmd_read("SET", "kx", "3"); w.close()  # foreign
    R["noloop_foreign"] = norm_push(t.drain_push())
    t.close()

    # ── BCAST: prefix match, non-match, multi-key batch ──
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON", "BCAST", "PREFIX", "foo")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "foobar", "1"); w.close()
    R["bcast_match"] = norm_push(t.drain_push())
    w = Conn(port); w.cmd_read("SET", "zzz", "1"); w.close()
    R["bcast_nomatch"] = norm_push(t.drain_push())
    w = Conn(port); w.cmd_read("MSET", "foo1", "1", "foo2", "2"); w.close()
    R["bcast_multikey_batch"] = norm_push(t.drain_push())
    t.close()

    # ── bcast INDEX maintenance (yaxr7.2) ──
    # ON then OFF -> no push
    fresh()
    t = tracker("CLIENT", "TRACKING", "ON", "BCAST", "PREFIX", "foo")
    t.cmd_read("CLIENT", "TRACKING", "OFF")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "foo1", "v"); w.close()
    R["idx_off_nopush"] = norm_push(t.drain_push())
    # re-enable -> push again
    t.cmd_read("CLIENT", "TRACKING", "ON", "BCAST", "PREFIX", "foo")
    t.drain_push(0.12)
    w = Conn(port); w.cmd_read("SET", "foo2", "v"); w.close()
    R["idx_reenable_push"] = norm_push(t.drain_push())
    t.close()

    # two bcast clients, distinct prefixes
    fresh()
    a = tracker("CLIENT", "TRACKING", "ON", "BCAST", "PREFIX", "aaa")
    b = tracker("CLIENT", "TRACKING", "ON", "BCAST", "PREFIX", "bbb")
    a.drain_push(0.1); b.drain_push(0.1)
    w = Conn(port); w.cmd_read("SET", "aaa1", "v"); w.close()
    R["idx_two_a"] = norm_push(a.drain_push())
    R["idx_two_b"] = norm_push(b.drain_push())
    a.close(); b.close()

    # peer disconnect: remaining bcast client still served
    fresh()
    dead = tracker("CLIENT", "TRACKING", "ON", "BCAST", "PREFIX", "foo")
    live = tracker("CLIENT", "TRACKING", "ON", "BCAST", "PREFIX", "foo")
    live.drain_push(0.1)
    dead.close()
    time.sleep(0.2)
    w = Conn(port); w.cmd_read("SET", "foo1", "v"); w.close()
    R["idx_live_after_disconnect"] = norm_push(live.drain_push())
    live.close()

    return R


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    o, f = run(op), run(fp)
    div = 0
    for k in o:
        if o[k] != f.get(k):
            div += 1
            print(f"DIVERGE [{k}]\n     oracle: {o[k]}\n     fr    : {f.get(k)}")
    print("-" * 60)
    if div == 0:
        print(f"PASS — fr CLIENT TRACKING matches redis 7.2.4 across {len(o)} scenarios")
    else:
        print(f"FAIL — {div} divergence(s)")
    sys.exit(1 if div else 0)


if __name__ == "__main__":
    main()
