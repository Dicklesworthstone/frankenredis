#!/usr/bin/env python3
"""Differential guard for the WRITE-path facet of the LFU->LRU OBJECT IDLETIME
reinterpretation (frankenredis-qwqln), fr vs vendored redis 7.2.4.

The 97wc2 fix made non-LFU READ accesses clear the stale LFU clock marker (so
OBJECT IDLETIME tracks the fresh access). But in-place WRITES (APPEND, SETRANGE,
...) go through Entry::touch_write, which does NOT clear the marker, so after a
LFU->non-LFU policy switch a write leaves IDLETIME reporting the stale
reinterpreted value instead of ~0. redis's lookupKeyWrite under a non-LFU policy
sets robj.lru = LRU_CLOCK(), so IDLETIME is ~0.

This is a sibling/superset of lfu_idletime_policy_differ.py (READ path, owned by
another agent); kept standalone so the write facet is guarded independently. The
diverging cases are DOCUMENTED (NOTE, gate stays green) and AUTO-PROMOTE to a
HARD failure once qwqln lands the policy-aware write touch — at which point this
file (or its cases) can fold into the read gate.

Usage: lfu_idletime_write_reaccess_differ.py <oracle_port> <fr_port>
       Exit 0 = parity (modulo documented qwqln divergences), 1 = NEW divergence,
            2 = setup error. Both servers need --enable-debug-command.
"""
import socket
import sys


class R:
    def __init__(self, p):
        self.s = socket.create_connection(("127.0.0.1", p), timeout=10)
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


def big(x):
    try:
        return as_int(x) > 1
    except (TypeError, ValueError):
        return False


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = R(op), R(fp)

    # Preflight: OBJECT IDLETIME needs neither DEBUG nor RESP3, but a server that
    # rejects CONFIG SET maxmemory-policy would invalidate the probe.
    for nm, d in (("oracle", od), ("fr", fr)):
        rep = d.cmd("config", "set", "maxmemory-policy", "noeviction")
        if "OK" not in rep:
            print(f"SETUP ERROR: {nm} CONFIG SET maxmemory-policy failed: {rep!r}")
            sys.exit(2)

    fails, notes = [], []

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
        if small(o) and small(f):
            notes.append(f"{label} now MATCHES (frankenredis-qwqln fixed?) — promote to HARD")
        elif small(o) and big(f):
            notes.append(
                f"{label} KNOWN DIVERGENCE (frankenredis-qwqln): redis={o} fr={f} "
                "(non-LFU in-place write must clear the LFU-bits reinterpretation)"
            )
        else:
            fails.append(f"{label} UNEXPECTED: redis={o!r} fr={f!r}")

    write_reaccess("append_reaccess", "append", "k", "x")
    write_reaccess("setrange_reaccess", "setrange", "k", "0", "Z")
    write_reaccess("setbit_reaccess", "setbit", "k", "0", "1")

    print("=" * 60)
    for n in notes:
        print(f"NOTE  {n}")
    if fails:
        print(f"FAIL — {len(fails)} NEW divergence(s):")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — LFU->LRU write-reaccess IDLETIME vs redis 7.2.4 "
        f"({len(notes)} documented qwqln divergence(s))"
    )


if __name__ == "__main__":
    main()
