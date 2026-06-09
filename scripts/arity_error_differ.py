#!/usr/bin/env python3
"""Differential gate: wrong-arity error wording across the WHOLE command table.

Walks every command name reported by `COMMAND LIST` and invokes it under-arity
(zero extra args, then one extra arg), asserting fr's error reply matches
vendored redis 7.2.4 byte for byte. This catches the shared-handler / alias
mis-naming class — e.g. GEORADIUS_RO / GEORADIUSBYMEMBER_RO / RESTORE-ASKING
once reported the BASE command's name in their arity error
(frankenredis-cmqwr), which a per-command hand-probe list misses but a
full-table sweep finds.

Stateful / dangerous / nondeterministic commands are skipped (they would kill a
server, block, or vary run-to-run). Only replies where at least one side is an
error (`-...`) are compared, so valid no-op invocations don't create noise.

SETUP (both servers config-LESS so the table aligns):
  ORACLE=legacy_redis_code/redis/src
  $ORACLE/redis-server --port 17851 --daemonize yes --save '' --appendonly no
  $CARGO_TARGET_DIR/debug/frankenredis --port 17852 --mode strict &
  scripts/arity_error_differ.py --oracle 17851 --fr 17852

Exit status: 0 = byte-exact, 1 = at least one divergence (details printed).
"""
import argparse
import socket
import sys

# Commands that would crash a server, block the connection, mutate global state
# destructively, or return nondeterministic content — excluded from the sweep.
SKIP = {
    "shutdown", "debug", "failover", "save", "bgsave", "bgrewriteaof", "monitor",
    "subscribe", "psubscribe", "ssubscribe", "sync", "psync", "replicaof",
    "slaveof", "reset", "quit", "multi", "exec", "discard", "watch", "unwatch",
    "blpop", "brpop", "blmove", "blmpop", "brpoplpush", "bzpopmin", "bzpopmax",
    "bzmpop", "xread", "xreadgroup", "wait", "waitaof",
    # nondeterministic content (compared elsewhere) — not arity-relevant here:
    "info", "lolwut", "command", "client", "cluster", "memory", "latency",
    "slowlog", "acl", "config", "object", "xinfo", "xgroup", "pubsub", "function",
    "script", "randomkey",
}


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.buf = b""

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            a = a if isinstance(a, bytes) else str(a).encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self._read()

    def _read(self):
        while b"\r\n" not in self.buf:
            self.buf += self.s.recv(65536)
        line, self.buf = self.buf.split(b"\r\n", 1)
        t, rest = line[:1], line[1:]
        if t in (b"+", b"-", b":"):
            return line
        if t == b"$":
            n = int(rest)
            if n < 0:
                return b"$-1"
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(65536)
            d = self.buf[:n]
            self.buf = self.buf[n + 2:]
            return b"$" + d
        if t == b"*":
            n = int(rest)
            if n < 0:
                return line
            return line + b"|" + b"|".join(self._read() for _ in range(n))
        return line


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    o = Conn(args.oracle)
    f = Conn(args.fr)

    resp = o.cmd("COMMAND", "LIST")
    names = sorted({
        p[1:].decode(errors="replace")
        for p in resp.split(b"|")[1:]
        if p.startswith(b"$")
    })
    names = [n for n in names if n and n.isascii()]

    diffs = 0
    probed = 0
    for name in names:
        if name.lower() in SKIP:
            continue
        for extra in ([], ["k"]):
            ro = o.cmd(name, *extra)
            rf = f.cmd(name, *extra)
            probed += 1
            if ro != rf and (ro.startswith(b"-") or rf.startswith(b"-")):
                diffs += 1
                print(f"DIVERGE [{name} {' '.join(extra)}]\n  oracle={ro!r}\n  fr    ={rf!r}")

    if diffs:
        print(f"\nFAIL: {diffs} arity/error-wording divergence(s) over {probed} probes "
              f"({len(names)} commands)")
        sys.exit(1)
    print(f"OK: wrong-arity error wording byte-exact vs redis 7.2.4 over {probed} probes "
          f"({len(names)} commands, incl. _RO/-ASKING shared-handler aliases)")


if __name__ == "__main__":
    main()
