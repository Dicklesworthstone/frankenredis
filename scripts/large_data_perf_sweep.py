#!/usr/bin/env python3
"""Heavy-command perf regression guard: fr vs vendored redis 7.2.4.

Unlike the pipelined small-command throughput benchmarks (dominated by
per-command CPU + box noise), this times SINGLE large-data operations where
wall-clock == the underlying algorithm. fr is currently faster-or-equal to
redis on every heavy command measured here (often 2-20x faster: SORT, the
ZSET/SET algebra-STORE family, LPOS), thanks to the order-statistic treap,
borrow-scan set algebra, chunked-list seek, and bbox-pruned geo work. The only
sub-parity ops are memory-bandwidth-bound full materializations (ZRANGE 0 -1,
GETRANGE 0 -1) and the large-list compare scan (LREM) — all < 1.15x, where a
2x is physically impossible in safe Rust (you cannot beat memcpy/sequential
compare bandwidth).

This script LOCKS THAT LEAD IN: it FAILS if fr regresses past `THRESHOLD` on
any heavy op. Run it after any change to fr-store collection algorithms.

Setup (compiled defaults so configs align):
  ORACLE=legacy_redis_code/redis/src
  $ORACLE/redis-server --port 16801 --daemonize yes --save '' --appendonly no
  <fr-binary> --port 16800
  scripts/large_data_perf_sweep.py 16801 16800

Exit 0 = no regression; 1 = fr slower than THRESHOLD x redis on some op.
"""
import socket, sys, time

THRESHOLD = 1.30  # fail if fr is slower than 1.30x redis on a heavy op


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=60)
        self.buf = b""

    def _recv(self):
        d = self.s.recv(65536)
        if not d:
            raise EOFError
        self.buf += d


def send(c, *args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str):
            a = a.encode()
        elif not isinstance(a, bytes):
            a = str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    c.s.sendall(out)


def readone(c):
    while b"\r\n" not in c.buf:
        c._recv()
    line, c.buf = c.buf.split(b"\r\n", 1)
    t = line[:1]
    if t in (b"+", b"-", b":"):
        return line
    if t == b"$":
        n = int(line[1:])
        if n < 0:
            return line
        while len(c.buf) < n + 2:
            c._recv()
        d = c.buf[:n]
        c.buf = c.buf[n + 2:]
        return d
    if t == b"*":
        n = int(line[1:])
        if n < 0:
            return line
        return [readone(c) for _ in range(n)]
    return line


def pipe(c, cmds):
    for x in cmds:
        send(c, *x)
    return [readone(c) for _ in cmds]


def timed(c, *a):
    send(c, *a)
    t0 = time.perf_counter()
    readone(c)
    return time.perf_counter() - t0


def best(c, *a, n=3):
    return min(timed(c, *a) for _ in range(n))


def setup(c):
    pipe(c, [["flushall"]])
    pipe(c, [["rpush", "biglist"] + [f"e{i % 1000}" for i in range(j, j + 1000)]
             for j in range(0, 200000, 1000)])
    pipe(c, [["zadd", "bigz"] + sum([[str(i % 9973), f"m{i}"] for i in range(j, j + 500)], [])
             for j in range(0, 200000, 500)])
    pipe(c, [["zadd", "z1"] + sum([[str(i), f"a{i}"] for i in range(j, j + 500)], [])
             for j in range(0, 150000, 500)])
    pipe(c, [["zadd", "z2"] + sum([[str(i), f"b{i}"] for i in range(j, j + 500)], [])
             for j in range(0, 150000, 500)])
    pipe(c, [["sadd", "s1"] + [f"x{i}" for i in range(j, j + 1000)]
             for j in range(0, 150000, 1000)])
    pipe(c, [["sadd", "s2"] + [f"x{i}" for i in range(j + 50000, j + 51000)]
             for j in range(0, 150000, 1000)])
    pipe(c, [["set", "bigstr", "A" * 500000]])


TESTS = [
    ("SORT biglist ALPHA LIMIT 0 50", ["sort", "biglist", "ALPHA", "LIMIT", "0", "50"]),
    ("SORT biglist LIMIT 0 50", ["sort", "biglist", "LIMIT", "0", "50"]),
    ("LPOS biglist e500 COUNT 0", ["lpos", "biglist", "e500", "COUNT", "0"]),
    ("LREM biglist 0 nomatch", ["lrem", "biglist", "0", "nomatch_xyz"]),
    ("ZRANGEBYSCORE bigz 100 200", ["zrangebyscore", "bigz", "100", "200"]),
    ("ZUNIONSTORE 2 z1 z2", ["zunionstore", "dst", "2", "z1", "z2"]),
    ("ZINTERSTORE 2 z1 z2", ["zinterstore", "dst", "2", "z1", "z2"]),
    ("ZDIFFSTORE 2 z1 z2", ["zdiffstore", "dst", "2", "z1", "z2"]),
    ("SINTERSTORE s1 s2", ["sinterstore", "dst", "s1", "s2"]),
    ("SUNIONSTORE s1 s2", ["sunionstore", "dst", "s1", "s2"]),
    ("SDIFFSTORE s1 s2", ["sdiffstore", "dst", "s1", "s2"]),
    ("SMEMBERS s1", ["smembers", "s1"]),
    ("GETRANGE bigstr 0 -1", ["getrange", "bigstr", "0", "-1"]),
    ("ZRANGE bigz 0 -1", ["zrange", "bigz", "0", "-1"]),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16801
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16800
    od, fr = Conn(op), Conn(fp)
    for c in (od, fr):
        setup(c)
    print(f"{'op':32} {'redis_ms':>9} {'fr_ms':>9} {'fr/redis':>8}")
    worst = 0.0
    worst_op = None
    for name, cmd in TESTS:
        r = best(od, *cmd) * 1000
        f = best(fr, *cmd) * 1000
        ratio = f / r if r > 0 else 0.0
        flag = "  REGRESSION" if ratio > THRESHOLD else ""
        print(f"{name:32} {r:9.2f} {f:9.2f} {ratio:8.2f}{flag}")
        if ratio > worst:
            worst, worst_op = ratio, name
    print("-" * 60)
    if worst > THRESHOLD:
        print(f"FAIL — fr regressed to {worst:.2f}x redis on '{worst_op}' (> {THRESHOLD})")
        return 1
    print(f"PASS — fr faster-or-within-{THRESHOLD}x redis on all {len(TESTS)} heavy ops "
          f"(worst {worst:.2f}x on '{worst_op}')")
    return 0


if __name__ == "__main__":
    sys.exit(main())
