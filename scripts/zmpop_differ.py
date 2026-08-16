#!/usr/bin/env python3
"""frankenredis-9u5z9: three-way ZMPOP equivalence — fr fast route vs fr generic
path vs live Redis 7.2.4.

This is the scoped equivalent of `borrowed_fast_routes_agree_with_generic_dispatch_
and_legacy_redis` for the ZMPOP rows only. It exists because that gate asserts on
its FIRST mismatch and is currently RED at reply 110 on an unrelated HRANDFIELD
ordering bug (frankenredis-brs56), which masks every later row including these.

The fr arms are the SAME ELF: FR_PERF_AB_CASCADE_BYPASS=1 selects the generic path,
unset selects the front-classified fast route. So a difference between them is the
route, not the build.

    zmpop_differ.py <redis_port> <fr_fast_port> <fr_generic_port>
Exit 0 = all three agree on every case.
"""
import socket
import sys

RS, FF, FG = (int(a) for a in sys.argv[1:4])


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 10)
        self.buf = b""

    def _enc(self, args):
        out = [b"*%d\r\n" % len(args)]
        for a in args:
            b = a.encode() if isinstance(a, str) else a
            out.append(b"$%d\r\n%s\r\n" % (len(b), b))
        return b"".join(out)

    def _line(self):
        while b"\r\n" not in self.buf:
            c = self.s.recv(1 << 20)
            if not c:
                raise EOFError
            self.buf += c
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag in (b"+", b":"):
            return tag.decode() + rest.decode()
        if tag == b"-":
            return "ERR:" + rest.decode()
        if tag == b"$":
            n = int(rest)
            if n == -1:
                return "(nil)"
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(1 << 20)
            v, self.buf = self.buf[:n], self.buf[n + 2:]
            return v.decode(errors="replace")
        if tag == b"*":
            n = int(rest)
            if n == -1:
                return "(nil-array)"
            return "[" + ",".join(str(self._read()) for _ in range(n)) + "]"
        raise RuntimeError(f"bad tag {line!r}")

    def cmd(self, *a):
        self.s.sendall(self._enc(list(a)))
        return self._read()


# (command, ...) applied in order to all three engines; every reply compared.
CASES = [
    ("FLUSHALL",),
    ("ZADD", "z:mp", "1", "a", "2", "b", "3", "c", "4", "d"),
    ("SADD", "s:1", "x"),
    # The classified shape, both directions. MIN must take `a`, MAX must take `d`;
    # a route wired to the wrong executor passes a nil-only corpus and fails here.
    ("ZMPOP", "1", "z:mp", "MIN"),
    ("ZMPOP", "1", "z:mp", "MAX"),
    ("ZRANGE", "z:mp", "0", "-1", "WITHSCORES"),
    # The missing-key branch: the shape the 0.7698x/0.7860x loss was measured on.
    ("ZMPOP", "1", "z:absent", "MIN"),
    # Arities the classifier deliberately does NOT claim — must still reach the
    # cascade and answer identically.
    ("ZMPOP", "1", "z:mp", "MIN", "COUNT", "2"),
    ("ZMPOP", "2", "z:absent", "z:mp", "MIN"),
    # Errors must come from the generic path verbatim.
    ("ZMPOP", "1", "s:1", "MIN"),
    ("ZMPOP", "1", "z:mp", "SIDEWAYS"),
    ("ZMPOP", "0", "z:mp", "MIN"),
    ("ZMPOP", "1", "z:mp"),
    # Re-seed and re-run the classified shape so the pop is observed twice.
    ("ZADD", "z:mp2", "10", "p", "20", "q"),
    ("ZMPOP", "1", "z:mp2", "MIN"),
    ("ZMPOP", "1", "z:mp2", "MAX"),
    ("ZMPOP", "1", "z:mp2", "MIN"),
    ("EXISTS", "z:mp2"),
]

redis, fast, generic = Conn(RS), Conn(FF), Conn(FG)
bad = 0
print(f"{'case':<38} {'fr fast':<22} {'fr generic':<22} {'redis 7.2.4'}")
print("-" * 108)
for case in CASES:
    r = redis.cmd(*case)
    f = fast.cmd(*case)
    g = generic.cmd(*case)
    label = " ".join(case)
    ok = (f == g) and (f == r)
    if not ok:
        bad += 1
    mark = "" if ok else ("   <-- FAST != GENERIC" if f != g else "   <-- fr != 7.2.4")
    print(f"{label:<38} {f[:21]:<22} {g[:21]:<22} {r[:21]}{mark}")

print()
print(f"{len(CASES)} cases, {bad} disagreement(s)")
sys.exit(0 if bad == 0 else 1)
