#!/usr/bin/env python3
"""stream_cg_reload_gate.py — stream consumer-group state survives DEBUG RELOAD
byte-exact vs redis 7.2.4.

Builds a stream with multiple groups, consumers, a partially-ACKed PEL, an XDEL
tombstone of a still-pending entry, and explicit XSETID metadata
(ENTRIESADDED / MAXDELETEDID), then DEBUG RELOADs both fr and redis and compares
the post-reload projections cross-implementation. This guards the stream RDB
round-trip (STREAM_LISTPACKS_3 + consumer-group / PEL serialization — beads
sq4ov / sy2i3 / wt4eo) against regressions that single-command probes miss.

Elapsed-time fields (consumer idle/inactive, PEL delivery time) are masked: they
depend on wall-clock and are not part of the persisted logical state. Everything
stable — group/consumer names, pending counts, PEL membership (id, consumer,
delivery-count), last-delivered-id, entries-read, lag, length, last-generated-id,
max-deleted-entry-id, entries-added, recorded-first-entry-id, and the live
entries — must match byte-for-byte after each server reloads its own RDB.

Self-launches a clean fr + redis pair. Usage: [--bin FR] [--redis-bin REDIS]
"""
import argparse, os, socket, subprocess, sys, tempfile, time


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
        if t == b"+": return r.decode("latin1")
        if t == b"-": return "ERR:" + r.decode("latin1")
        if t == b":": return int(r)
        if t == b"$":
            n = int(r)
            if n < 0: return None
            while len(self.b) < n + 2: self.b += self.s.recv(65536)
            d, self.b = self.b[:n], self.b[n+2:]; return d.decode("latin1")
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.rd() for _ in range(n)]
        return line.decode("latin1")
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o); return self.rd()


def build(c):
    c.cmd("FLUSHALL")
    for i in range(1, 11):
        c.cmd("XADD", "st", f"{i}-0", "f", f"v{i}")
    c.cmd("XGROUP", "CREATE", "st", "g1", "0")
    c.cmd("XGROUP", "CREATE", "st", "g2", "5-0")
    c.cmd("XREADGROUP", "GROUP", "g1", "c1", "COUNT", "3", "STREAMS", "st", ">")
    c.cmd("XREADGROUP", "GROUP", "g1", "c2", "COUNT", "2", "STREAMS", "st", ">")
    c.cmd("XACK", "st", "g1", "2-0")
    c.cmd("XDEL", "st", "3-0")  # tombstone a still-pending entry
    c.cmd("XSETID", "st", "100-0", "ENTRIESADDED", "50", "MAXDELETEDID", "3-0")
    c.cmd("XGROUP", "CREATECONSUMER", "st", "g2", "lonely")


def stream_info_stable(c):
    """XINFO STREAM (non-FULL) without the time-sensitive / encoding-internal
    fields — keeps the logical metadata that must survive reload."""
    flat = c.cmd("XINFO", "STREAM", "st")
    d = {}
    for i in range(0, len(flat) - 1, 2):
        d[flat[i]] = flat[i + 1]
    keep = ("length", "last-generated-id", "max-deleted-entry-id",
            "entries-added", "recorded-first-entry-id", "groups")
    return {k: d.get(k) for k in keep}


def consumers_stable(c, group):
    out = []
    for con in c.cmd("XINFO", "CONSUMERS", "st", group):
        d = {}
        for i in range(0, len(con) - 1, 2):
            d[con[i]] = con[i + 1]
        out.append((d.get("name"), d.get("pending")))   # drop idle / inactive
    return out


def groups_stable(c):
    out = []
    for g in c.cmd("XINFO", "GROUPS", "st"):
        d = {}
        for i in range(0, len(g) - 1, 2):
            d[g[i]] = g[i + 1]
        out.append((d.get("name"), d.get("consumers"), d.get("pending"),
                    d.get("last-delivered-id"), d.get("entries-read"), d.get("lag")))
    return out


def pending_full(c, group):
    rows = c.cmd("XPENDING", "st", group, "-", "+", "100")
    # [id, consumer, idle-ms, delivery-count] -> drop idle-ms (index 2)
    return [(r[0], r[1], r[3]) for r in rows]


def snapshot(c):
    return {
        "xlen": c.cmd("XLEN", "st"),
        "xrange": c.cmd("XRANGE", "st", "-", "+"),
        "stream_info": stream_info_stable(c),
        "groups": groups_stable(c),
        "consumers_g1": consumers_stable(c, "g1"),
        "consumers_g2": consumers_stable(c, "g2"),
        "pending_g1": c.cmd("XPENDING", "st", "g1"),         # summary
        "pending_g1_full": pending_full(c, "g1"),
        "pending_g2": c.cmd("XPENDING", "st", "g2"),
    }


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") == "PONG": return True
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

    rdir = tempfile.mkdtemp(prefix="fr_streamcgreload_")
    fp, rp = free_port(), free_port()
    procs = [
        subprocess.Popen([fr, "--port", str(fp), "--enable-debug-command", "yes"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen([redis, "--port", str(rp), "--dir", rdir, "--save", "",
                          "--appendonly", "no", "--enable-debug-command", "yes"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        snaps = {}
        for name, port in (("fr", fp), ("rd", rp)):
            c = Conn(port)
            build(c)
            c.cmd("DEBUG", "RELOAD")
            snaps[name] = snapshot(c)
    finally:
        for p in procs: p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()

    diffs = []
    for k in snaps["fr"]:
        if snaps["fr"][k] != snaps["rd"][k]:
            diffs.append((k, snaps["fr"][k], snaps["rd"][k]))
    for k, a, b in diffs:
        print(f"  [DIFF] {k}\n    fr={a}\n    rd={b}")
    if diffs:
        print(f"FAIL — {len(diffs)} stream-CG reload divergence(s) vs redis 7.2.4")
        return 1
    print("PASS — stream consumer-group state survives DEBUG RELOAD byte-exact "
          "vs redis 7.2.4 (9 projections)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
