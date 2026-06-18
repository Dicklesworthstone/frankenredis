#!/usr/bin/env python3
"""info_stats_differ.py — INFO logical-stats differential vs redis 7.2.4.

Runs an identical mixed workload (writes of every type, reads, hits/misses,
errors, expiry, multi-key ops) on fr and on vendored redis after CONFIG
RESETSTAT, then compares the deterministic logical INFO stat fields.

keyspace_misses FIXED (ljtdo BUG1): XADD/XDEL/XGROUP CREATE|SETID resolved their
last-id / existence via a recording lookup, spuriously bumping keyspace_hits/misses
on a write path. Upstream resolves them via lookupKeyWrite, where LOOKUP_WRITE
suppresses the stat. fr now uses no-stat variants (Store::xlast_id_no_stat).

Previously-known total_reads_processed / total_writes_processed gaps are fixed;
every checked logical stat now hard-fails on Redis-vs-fr drift.

Excluded entirely: rdb_changes_since_last_save (cumulative since last SAVE, not
reset by RESETSTAT, so the absolute baselines differ between servers — not a
logical-stat divergence), plus all memory/cpu/time/version/pid env fields.

Usage: info_stats_differ.py <oracle_port> <fr_port>
"""
import socket, sys, time


class C:
    def __init__(self, p):
        self.s = socket.create_connection(("127.0.0.1", p), 3); self.s.settimeout(6); self.b = b""
    def _l(self):
        while b"\r\n" not in self.b: self.b += self.s.recv(65536)
        l, self.b = self.b.split(b"\r\n", 1); return l
    def _n(self, n):
        while len(self.b) < n + 2: self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n+2:]; return d
    def p(self):
        l = self._l(); t, r = l[:1], l[1:]
        if t == b"$":
            n = int(r); return None if n < 0 else self._n(n)
        if t == b":": return int(r)
        if t in (b"+", b"-"): return r
        if t == b"*":
            n = int(r); return None if n < 0 else [self.p() for _ in range(n)]
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o); return self.p()


def info(c):
    d = {}
    for sec in ("stats", "persistence", "keyspace", "clients"):
        t = c.cmd("INFO", sec).decode(errors="replace")
        for line in t.splitlines():
            if ":" in line and not line.startswith("#"):
                k, v = line.split(":", 1); d[k.strip()] = v.strip()
    return d


# Deterministic logical fields (no memory/cpu/time/version, no cumulative dirty).
FIELDS = [
    "total_reads_processed", "total_writes_processed", "expired_keys",
    "expired_subkeys", "keyspace_hits", "keyspace_misses", "total_error_replies",
    "unexpected_error_replies", "rejected_connections", "sync_full",
    "sync_partial_ok", "sync_partial_err", "pubsub_channels", "pubsub_patterns",
    "total_net_output_bytes", "total_net_input_bytes", "db0", "blocked_clients",
    "watching_clients", "tracking_clients", "total_forks", "evicted_keys",
]

def workload(c):
    c.cmd("FLUSHALL")
    c.cmd("CONFIG", "RESETSTAT")
    c.cmd("SET", "s", "v"); c.cmd("INCR", "ctr"); c.cmd("APPEND", "s", "x")
    c.cmd("RPUSH", "l", "a", "b", "c"); c.cmd("LPOP", "l")
    c.cmd("SADD", "st", "a", "b"); c.cmd("SREM", "st", "a")
    c.cmd("HSET", "h", "f", "v"); c.cmd("HDEL", "h", "f")
    c.cmd("ZADD", "z", "1", "a", "2", "b"); c.cmd("ZREM", "z", "a")
    c.cmd("XADD", "stm", "*", "f", "1")
    c.cmd("GET", "s"); c.cmd("GET", "nope"); c.cmd("EXISTS", "s", "nope")
    c.cmd("LRANGE", "l", "0", "-1"); c.cmd("HGETALL", "h")
    c.cmd("TYPE", "s"); c.cmd("STRLEN", "s"); c.cmd("LLEN", "l")
    for bad in (("INCR", "l"), ("GET",), ("NOTACMD",), ("EXPIRE", "s", "notanum")):
        try: c.cmd(*bad)
        except Exception: pass
    c.cmd("SET", "e", "v", "PX", "50"); time.sleep(0.15); c.cmd("GET", "e")
    c.cmd("MSET", "m1", "a", "m2", "b"); c.cmd("MGET", "m1", "m2", "nope")
    c.cmd("DEL", "m1", "nope2")


def main():
    o, f = C(int(sys.argv[1])), C(int(sys.argv[2]))
    workload(o); workload(f)
    io, iff = info(o), info(f)
    divs = [(k, io.get(k, "<absent>"), iff.get(k, "<absent>"))
            for k in FIELDS if io.get(k, "<absent>") != iff.get(k, "<absent>")]
    for k, a, b in divs:
        print(f"  {k:28s} redis={a!r} fr={b!r}")
    if divs:
        print(f"FAIL — {len(divs)} INFO-stat divergence(s) vs redis 7.2.4")
        return 1
    print("PASS — INFO logical stats match redis 7.2.4")
    return 0


if __name__ == "__main__":
    sys.exit(main())
