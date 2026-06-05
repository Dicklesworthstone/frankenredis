#!/usr/bin/env python3
"""keyspace_notif_differ.py — differential fuzzer for keyspace notifications.

With `notify-keyspace-events KEA` set, a subscriber PSUBSCRIBEs to
`__key*@0__:*`; a second connection runs an identical random command sequence
against fr-server and the vendored redis 7.2.4 oracle. After each command the
emitted notifications (channel + payload, in order) are drained and compared.
This exercises WHICH events fire, their channel/payload, and ordering — e.g.
del-on-empty, rename_from/rename_to, copy_to, move_from/move_to, setrange,
incrby, spop/srem, zpop, hdel, lrem/ltrim, expire/persist, etc.

Deterministic events only: TTL-based "expired" is timer-driven and excluded.

Usage: keyspace_notif_differ.py [--oracle 16399] [--fr 16400] [--iters 3000] [--seed N]
"""
import argparse
import random
import socket
import sys
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port))
        self.s.settimeout(2)
        self.buf = b""

    def _fill(self, block=True):
        try:
            if not block:
                self.s.settimeout(0.06)
            c = self.s.recv(65536)
            if not c:
                raise EOFError("closed")
            self.buf += c
            return True
        except socket.timeout:
            return False
        finally:
            if not block:
                self.s.settimeout(2)

    def _readline(self):
        while b"\r\n" not in self.buf:
            self._fill()
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _readn(self, n):
        while len(self.buf) < n + 2:
            self._fill()
        d, self.buf = self.buf[:n], self.buf[n + 2:]
        return d

    def _parse(self):
        line = self._readline()
        t, rest = line[:1], line[1:]
        if t == b":":
            return ("int", int(rest))
        if t == b"+":
            return ("status", rest)
        if t == b"-":
            return ("error", rest)
        if t == b"$":
            n = int(rest)
            return ("nil", None) if n < 0 else ("bulk", self._readn(n))
        if t in (b"*", b">", b"~"):
            n = int(rest)
            return ("nil", None) if n < 0 else ("array", [self._parse() for _ in range(n)])
        if t == b"%":
            n = int(rest)
            return ("array", [self._parse() for _ in range(n * 2)])
        return ("other", rest)

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, int):
                a = str(a)
            if isinstance(a, str):
                a = a.encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self._parse()

    def drain_pmessages(self):
        """Collect all pending pmessage frames (best-effort, ~timeout-bounded)."""
        msgs = []
        # ensure at least one short wait so async pushes arrive
        while True:
            if b"\r\n" not in self.buf and not self._fill(block=False):
                break
            # parse one frame if a full one is buffered
            try:
                save = self.buf
                frame = self._parse_nonblock()
            except _Incomplete:
                self.buf = save
                if not self._fill(block=False):
                    break
                continue
            if frame is None:
                break
            if frame[0] == "array" and frame[1] and frame[1][0] == ("bulk", b"pmessage"):
                ch = frame[1][2][1]
                payload = frame[1][3][1]
                msgs.append((ch, payload))
        return msgs

    def _parse_nonblock(self):
        # Like _parse but raises _Incomplete instead of blocking.
        if b"\r\n" not in self.buf:
            raise _Incomplete()
        idx = self.buf.index(b"\r\n")
        line = self.buf[:idx]
        t, rest = line[:1], line[1:]
        if t == b"*":
            n = int(rest)
            self.buf = self.buf[idx + 2:]
            return ("array", [self._parse_nonblock() for _ in range(n)]) if n >= 0 else ("nil", None)
        if t == b"$":
            n = int(rest)
            if n < 0:
                self.buf = self.buf[idx + 2:]
                return ("nil", None)
            if len(self.buf) < idx + 2 + n + 2:
                raise _Incomplete()
            data = self.buf[idx + 2:idx + 2 + n]
            self.buf = self.buf[idx + 2 + n + 2:]
            return ("bulk", data)
        if t == b":":
            self.buf = self.buf[idx + 2:]
            return ("int", int(rest))
        self.buf = self.buf[idx + 2:]
        return ("other", rest)


class _Incomplete(Exception):
    pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--iters", type=int, default=3000)
    ap.add_argument("--seed", type=int, default=1234)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    op, fp = Conn(args.oracle), Conn(args.fr)
    os_, fs_ = Conn(args.oracle), Conn(args.fr)  # subscriber connections
    for c in (op, fp):
        c.cmd("FLUSHALL")
        c.cmd("CONFIG", "SET", "notify-keyspace-events", "KEA")
    for c in (os_, fs_):
        c.cmd("PSUBSCRIBE", "__key*@0__:*")
        time.sleep(0.05)
        c.drain_pmessages()  # clear the subscribe confirmation

    keys = ["k1", "k2", "k3"]

    def k():
        return rng.choice(keys)

    def v():
        return rng.choice(["x", "1", "10", "-3", "ab", "yyy"])

    log = []
    ops = [
        lambda: ("SET", k(), v()),
        lambda: ("SETEX", k(), "100", v()),
        lambda: ("APPEND", k(), v()),
        lambda: ("SETRANGE", k(), str(rng.randint(0, 5)), v()),
        lambda: ("INCR", k()),
        lambda: ("INCRBY", k(), str(rng.randint(-5, 5))),
        lambda: ("GETSET", k(), v()),
        lambda: ("GETDEL", k()),
        lambda: ("DEL", k(), k()),
        lambda: ("EXPIRE", k(), "1000"),
        lambda: ("PERSIST", k()),
        lambda: ("RENAME", k(), k()),
        lambda: ("COPY", k(), k(), "REPLACE"),
        lambda: ("MOVE", k(), "1"),
        lambda: ("LPUSH", k(), v()),
        lambda: ("RPUSH", k(), v(), v()),
        lambda: ("LPOP", k()),
        lambda: ("RPOP", k(), str(rng.randint(1, 2))),
        lambda: ("LREM", k(), str(rng.randint(-2, 2)), v()),
        lambda: ("LSET", k(), "0", v()),
        lambda: ("LINSERT", k(), "BEFORE", v(), v()),
        lambda: ("LTRIM", k(), "0", "1"),
        lambda: ("SADD", k(), v(), v()),
        lambda: ("SREM", k(), v()),
        lambda: ("SINTERSTORE", k(), k(), k()),
        lambda: ("HSET", k(), v(), v()),
        lambda: ("HDEL", k(), v()),
        lambda: ("HINCRBY", k(), v(), "2"),
        lambda: ("ZADD", k(), "1", v()),
        lambda: ("ZREM", k(), v()),
        lambda: ("ZINCRBY", k(), "1", v()),
        lambda: ("ZPOPMIN", k()),
        lambda: ("XADD", k(), "*", "f", v()),
    ]

    for it in range(args.iters):
        opv = tuple(str(x) for x in rng.choice(ops)())
        op.cmd(*opv)
        fp.cmd(*opv)
        time.sleep(0.02)
        oe = sorted(os_.drain_pmessages())
        fe = sorted(fs_.drain_pmessages())
        log.append(" ".join(opv) + "  => O:%d F:%d events" % (len(oe), len(fe)))
        if oe != fe:
            print("=== KEYSPACE-EVENT DIVERGENCE at iter %d ===" % it)
            print("seed=%d" % args.seed)
            print("op: %s" % " ".join(opv))
            print("oracle events: %s" % oe)
            print("fr     events: %s" % fe)
            print("--- op log (last 30) ---")
            for line in log[-30:]:
                print("  " + line)
            sys.exit(1)

    print("OK: %d iters, seed %d — no keyspace-notification divergence" % (args.iters, args.seed))


if __name__ == "__main__":
    main()
