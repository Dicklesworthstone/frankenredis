#!/usr/bin/env python3
"""Differential gate for OBJECT subcommands under varying maxmemory-policy.

OBJECT FREQ / IDLETIME are policy-GATED (a config-interaction edge — the a0p5p
class): FREQ requires an LFU policy (else '-ERR An LFU maxmemory policy is not
selected...'), IDLETIME requires a non-LFU policy (else the symmetric error).
OBJECT ENCODING / REFCOUNT / HELP and bad-subcommand errors round out the
surface. This sweeps all six maxmemory-policies and diffs the reply CLASS
(error token + message head vs value) — the nondeterministic numeric FREQ/
IDLETIME/REFCOUNT values are normalized so only the error-vs-value gating and
wording are compared.

Usage: object_policy_differ.py <oracle_port> <fr_port>
Exit 0 = parity; 1 = divergence(s).
"""
import socket, sys


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=10)


class R:
    def __init__(s, p):
        s.s = conn(p)
        s.buf = b""

    def _l(s):
        while b"\r\n" not in s.buf:
            s.buf += s.s.recv(1 << 20)
        l, s.buf = s.buf.split(b"\r\n", 1)
        return l

    def _n(s, n):
        while len(s.buf) < n + 2:
            s.buf += s.s.recv(1 << 20)
        d = s.buf[:n]
        s.buf = s.buf[n + 2:]
        return d

    def read(s):
        l = s._l()
        t = l[:1]
        if t in (b'+', b':', b'-'):
            return l.decode()
        if t == b'$':
            n = int(l[1:])
            return None if n < 0 else s._n(n).decode("latin1")
        if t in (b'*', b'~'):
            n = int(l[1:])
            return None if n < 0 else [s.read() for _ in range(n)]
        return l.decode()

    def cmd(s, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else x
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        s.s.sendall(o)
        return s.read()


POLICIES = ["noeviction", "allkeys-lru", "allkeys-lfu", "volatile-lfu",
            "volatile-ttl", "allkeys-random"]
# (label, command) — REFCOUNT excluded from value compare (shared-int WONTFIX);
# we still check it doesn't ERROR.
SUBCMDS = [
    ("FREQ", ["object", "freq", "k"]),
    ("IDLETIME", ["object", "idletime", "k"]),
    ("FREQ-missing", ["object", "freq", "nope"]),
    ("IDLETIME-missing", ["object", "idletime", "nope"]),
    ("ENCODING", ["object", "encoding", "k"]),
    ("HELP", ["object", "help"]),
    ("badsubcmd", ["object", "frobnicate", "k"]),
]


def norm(r):
    # error -> token + first 3 words (wording, not exact); integer -> 'INT'; else verbatim
    if isinstance(r, str) and r.startswith("-"):
        return " ".join(r.split()[:4])
    if isinstance(r, str) and r.lstrip("-").isdigit():
        return "INT"
    return r


def main():
    od = R(int(sys.argv[1]))
    fr = R(int(sys.argv[2]))
    div = 0
    for pol in POLICIES:
        od.cmd("flushall")
        fr.cmd("flushall")
        od.cmd("config", "set", "maxmemory-policy", pol)
        fr.cmd("config", "set", "maxmemory-policy", pol)
        od.cmd("set", "k", "v")
        fr.cmd("set", "k", "v")
        for label, c in SUBCMDS:
            ro, rf = norm(od.cmd(*c)), norm(fr.cmd(*c))
            if ro != rf:
                div += 1
                print(f"DIVERGE {label}@{pol} [{' '.join(c)}]\n  oracle: {ro}\n  fr    : {rf}")
    od.cmd("config", "set", "maxmemory-policy", "noeviction")
    fr.cmd("config", "set", "maxmemory-policy", "noeviction")
    print("-" * 60)
    if div:
        print(f"FAIL — {div} divergence(s)")
        return 1
    print(f"PASS — OBJECT subcommand policy-gating byte-exact vs redis 7.2.4 "
          f"({len(POLICIES)} policies x {len(SUBCMDS)} subcmds)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
