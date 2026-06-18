#!/usr/bin/env python3
"""Differential guard for the WRITE-path facet of the LFU->LRU OBJECT IDLETIME
reinterpretation (frankenredis-qwqln), fr vs vendored redis 7.2.4.

The 97wc2 fix made non-LFU READ accesses clear the stale LFU clock marker (so
OBJECT IDLETIME tracks the fresh access). But in-place WRITES (APPEND, SETRANGE,
...) must clear the stale marker too, so after a LFU->non-LFU policy switch a
write leaves IDLETIME reporting ~0. redis's lookupKeyWrite under a non-LFU
policy sets robj.lru = LRU_CLOCK(), so IDLETIME is ~0.

This is a sibling/superset of lfu_idletime_policy_differ.py (READ path, owned by
another agent); kept standalone so the write facet is guarded independently.
APPEND, SETRANGE, and SETBIT are hard parity checks for frankenredis-qwqln.

Usage: lfu_idletime_write_reaccess_differ.py <oracle_port> <fr_port>
       Exit 0 = parity, 1 = divergence,
            2 = setup error. Both servers need --enable-debug-command.
"""
import socket
import sys


class R:
    def __init__(self, sock):
        self.s = sock
        self.buf = b""

    def _l(self):
        while b"\r\n" not in self.buf:
            self.buf += self.s.recv(1 << 20)
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _n(self, n):
        while len(self.buf) < n + 2:
            self.buf += self.s.recv(1 << 20)
        d = self.buf[:n]
        self.buf = self.buf[n + 2:]
        return d

    def read(self):
        line = self._l()
        t = line[:1]
        if t in (b"+", b":", b"-"):
            return line.decode("latin1")
        if t == b"$":
            n = int(line[1:])
            return None if n < 0 else self._n(n).decode("latin1")
        if t == b"*":
            n = int(line[1:])
            return None if n < 0 else [self.read() for _ in range(n)]
        return line.decode("latin1")

    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else x
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o)
        return self.read()


def as_int(x):
    if isinstance(x, str):
        x = x.lstrip(":+")
    return int(x)


def small(x):
    try:
        return as_int(x) <= 1
    except (TypeError, ValueError):
        return False


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    with socket.create_connection(("127.0.0.1", op), timeout=10) as od_sock, socket.create_connection(
        ("127.0.0.1", fp), timeout=10
    ) as fr_sock:
        od, fr = R(od_sock), R(fr_sock)
        # Preflight: OBJECT IDLETIME needs neither DEBUG nor RESP3, but a server
        # that rejects CONFIG SET maxmemory-policy would invalidate the probe.
        for nm, d in (("oracle", od), ("fr", fr)):
            rep = d.cmd("config", "set", "maxmemory-policy", "noeviction")
            if "OK" not in rep:
                print(f"SETUP ERROR: {nm} CONFIG SET maxmemory-policy failed: {rep!r}")
                sys.exit(2)

        fails = []

        def write_reaccess(label, *write_cmd):
            for d in (od, fr):
                d.cmd("config", "set", "maxmemory-policy", "allkeys-lfu")
                d.cmd("flushall")
            for d in (od, fr):
                d.cmd("set", "k", "v")
                d.cmd("get", "k")  # LFU access -> marks the LFU clock field
                d.cmd("config", "set", "maxmemory-policy", "noeviction")
                d.cmd(*write_cmd)  # in-place write under non-LFU policy
            o, f = od.cmd("object", "idletime", "k"), fr.cmd("object", "idletime", "k")
            if not (small(o) and small(f)):
                fails.append(
                    f"{label}: cmd={list(write_cmd)} redis={o!r} fr={f!r} "
                    "(non-LFU in-place write must clear the LFU-bits reinterpretation)"
                )

        write_reaccess("append_reaccess", "append", "k", "x")
        write_reaccess("setrange_reaccess", "setrange", "k", "0", "Z")
        write_reaccess("setbit_reaccess", "setbit", "k", "0", "1")

        print("=" * 60)
        if fails:
            print(f"FAIL — {len(fails)} LFU->LRU write-reaccess divergence(s):")
            for x in fails:
                print(f"  {x}")
            sys.exit(1)
        print(
            "PASS — LFU->LRU write-reaccess IDLETIME vs redis 7.2.4 "
            "(hard APPEND/SETRANGE/SETBIT checks)"
        )


if __name__ == "__main__":
    main()
