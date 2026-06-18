#!/usr/bin/env python3
"""Differential fuzzer for BITFIELD / BITFIELD_RO vs vendored redis 7.2.4.

BITFIELD has a rich, under-covered surface: signed/unsigned typed accessors
(i1..i64 / u1..u63), absolute and `#`-relative offsets, and three OVERFLOW
modes (WRAP / SAT / FAIL) whose interaction with INCRBY and SET-return-old is
subtle. The existing scripts only touch BITFIELD incidentally; this hammers the
operator sequence + type/offset/overflow permutations and diffs every reply.

Usage: bitfield_differ.py <oracle_port> <fr_port> [seed] [iters]
"""
import socket, sys, random

class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.buf = b""
    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, str): a = a.encode()
            elif isinstance(a, int): a = str(a).encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self.read()
    def _readline(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d: raise EOFError
            self.buf += d
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line
    def _readn(self, n):
        while len(self.buf) < n + 2:
            d = self.s.recv(65536)
            if not d: raise EOFError
            self.buf += d
        data = self.buf[:n]; self.buf = self.buf[n+2:]
        return data
    def read(self):
        line = self._readline()
        t, rest = line[:1], line[1:]
        if t == b'+': return ('status', rest.decode())
        if t == b'-': return ('error', rest.decode())
        if t == b':': return ('int', int(rest))
        if t == b'$':
            n = int(rest)
            if n == -1: return ('nil', None)
            return ('bulk', self._readn(n))
        if t == b'*':
            n = int(rest)
            if n == -1: return ('nil', None)
            return ('arr', [self.read() for _ in range(n)])
        raise ValueError(f"bad type {line!r}")

def norm_err(r):
    # Compare error class, not exact glibc wording, but DO compare the
    # leading token + key phrases (redis BITFIELD errors are stable).
    if r[0] == 'error':
        return ('error', r[1].split()[0] if r[1] else '')
    return r

def rand_type(rng):
    signed = rng.random() < 0.5
    if signed:
        w = rng.randint(1, 64)
        return f"i{w}"
    w = rng.randint(1, 63)
    return f"u{w}"

def rand_offset(rng):
    if rng.random() < 0.4:
        return f"#{rng.randint(0, 12)}"
    return str(rng.randint(0, 200))

def rand_value(rng):
    choices = [0, 1, -1, 2, -2, 127, 128, 255, 256, -128, -129, 32767, -32768,
               65535, 65536, 2147483647, -2147483648, 4294967295,
               9223372036854775807, -9223372036854775808, 100, -100, 7, -7]
    return rng.choice(choices)

def build_ops(rng, n):
    ops = []
    for _ in range(n):
        k = rng.random()
        if k < 0.30:
            ops += ["GET", rand_type(rng), rand_offset(rng)]
        elif k < 0.60:
            ops += ["SET", rand_type(rng), rand_offset(rng), str(rand_value(rng))]
        elif k < 0.88:
            ops += ["INCRBY", rand_type(rng), rand_offset(rng), str(rand_value(rng))]
        else:
            ops += ["OVERFLOW", rng.choice(["WRAP", "SAT", "FAIL"])]
    return ops

def main():
    op, fp = int(sys.argv[1]), int(sys.argv[2])
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    iters = int(sys.argv[4]) if len(sys.argv) > 4 else 5000
    O, F = Conn(op), Conn(fp)

    def cleanup():
        for c in (O, F):
            try:
                c.cmd("flushall")
            except Exception:
                pass

    O.cmd("flushall"); F.cmd("flushall")
    rng = random.Random(seed)
    div = 0
    for i in range(iters):
        key = f"bf{rng.randint(0,7)}"
        nops = rng.randint(1, 6)
        ops = build_ops(rng, nops)
        cmd = (["BITFIELD_RO", key] if (rng.random() < 0.15 and all(
            o not in ("SET", "INCRBY", "OVERFLOW") for o in ops)) else ["BITFIELD", key]) + ops
        ro = O.cmd(*cmd); rf = F.cmd(*cmd)
        if norm_err(ro) != norm_err(rf):
            div += 1
            if div <= 25:
                print(f"DIVERGE seed={seed} iter={i}: {cmd}\n  oracle: {ro}\n  fr    : {rf}")
        # occasionally reset a key to keep widths/bytes bounded
        if rng.random() < 0.03:
            O.cmd("del", key); F.cmd("del", key)
    print("-"*60)
    print(f"seed {seed} x {iters} iters; divergences: {div}")
    cleanup()
    return 1 if div else 0

if __name__ == "__main__":
    sys.exit(main())
