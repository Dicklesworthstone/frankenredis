#!/usr/bin/env python3
"""Cross-impl DUMP byte-equality gate: fr vs redis 7.2.4 oracle.

DUMP must produce BYTE-IDENTICAL output across implementations so that
MIGRATE / cluster / RESTORE interop works in both directions (a redis client
can RESTORE an fr DUMP and vice versa). The DEBUG DIGEST oracle checks fr's
INTERNAL consistency; this gate checks the on-wire serialization FORMAT against
redis. Uses only types whose DUMP is order-deterministic (strings, ints,
lists, intsets, small listpack zsets); hashtable-encoded hashes/sets DUMP in
dict-iteration order and are intentionally excluded.

Usage: dump_byte_equality_gate.py <oracle_port> <fr_port>
"""
import socket, sys, time

ORACLE = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
FR = int(sys.argv[2]) if len(sys.argv) > 2 else 16400


def enc(args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        a = a if isinstance(a, bytes) else str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port))
        self.s.settimeout(3)
        self.buf = b""

    def _fill(self, n):
        while len(self.buf) < n:
            d = self.s.recv(65536)
            if not d:
                break
            self.buf += d

    def _line(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d:
                break
            self.buf += d
        i = self.buf.index(b"\r\n")
        line, self.buf = self.buf[:i], self.buf[i + 2:]
        return line

    def cmd(self, args):
        self.s.sendall(enc(args))
        line = self._line()
        t = line[:1]
        if t in (b"+", b"-", b":"):
            return line + b"\r\n"
        if t == b"$":
            n = int(line[1:])
            if n < 0:
                return line + b"\r\n"
            self._fill(n + 2)
            body, self.buf = self.buf[:n], self.buf[n + 2:]
            return body  # raw payload (DUMP returns binary)
        return line + b"\r\n"


BUILDS = [
    ("str_embstr", ["SET", "str_embstr", "hello world"]),
    ("str_int", ["SET", "str_int", "123456"]),
    ("str_raw", ["SET", "str_raw", "y" * 100]),
    ("list_listpack", ["RPUSH", "list_listpack", "a", "b", "c"]),
    ("list_quicklist", ["RPUSH", "list_quicklist", *[str(i) for i in range(50)]]),
    ("list_quicklist_big", ["RPUSH", "list_quicklist_big", *[("v" * 30) for _ in range(200)]]),
    ("set_intset", ["SADD", "set_intset", "1", "2", "3", "4", "5"]),
    ("zset_listpack", ["ZADD", "zset_listpack", "1", "a", "2.5", "b", "-3", "c"]),
]


def main():
    R, F = Conn(ORACLE), Conn(FR)
    # Preflight: this gate's content oracle is DEBUG DIGEST, so a server launched
    # without --enable-debug-command rejects it and would surface as a phantom
    # byte/digest mismatch. Fail fast with a clear setup message (exit 2) instead.
    for nm, p, c in (("oracle", ORACLE, R), ("fr", FR, F)):
        rep = c.cmd(["DEBUG", "DIGEST"])
        if not (isinstance(rep, (bytes, bytearray)) and rep.startswith(b"+")):
            print(f"SETUP ERROR: {nm} (port {p}) DEBUG DIGEST unavailable: {rep!r}")
            print("  Launch both redis and fr with --enable-debug-command yes.")
            sys.exit(2)
    for c in (R, F):
        c.cmd(["FLUSHALL"])
        for _, build in BUILDS:
            c.cmd(build)
    failures = []
    for name, _ in BUILDS:
        rd = R.cmd(["DUMP", name])
        fd = F.cmd(["DUMP", name])
        if rd != fd:
            failures.append((name, rd, fd, None))
            continue
        # redis must accept fr's DUMP payload (cross-impl RESTORE)
        rr = R.cmd(["RESTORE", "xr_" + name, "0", fd])
        if b"OK" not in rr:
            failures.append((name, rd, fd, rr))
    print("=" * 60)
    if failures:
        print(f"FAIL — {len(failures)} DUMP divergence(s) vs redis 7.2.4:")
        for name, rd, fd, rr in failures[:20]:
            if rr is not None:
                print(f"  [{name}] byte-equal but redis RESTORE rejected: {rr!r}")
            else:
                print(f"  [{name}] redis={len(rd)}B fr={len(fd)}B\n"
                      f"    redis={rd[:48].hex()}\n    fr   ={fd[:48].hex()}")
        sys.exit(1)
    print(f"PASS — DUMP byte-identical vs redis 7.2.4 ({len(BUILDS)} types);"
          " redis RESTORE accepts every fr DUMP (cross-impl MIGRATE interop)")


main()
