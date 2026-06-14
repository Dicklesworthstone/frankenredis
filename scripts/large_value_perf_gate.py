#!/usr/bin/env python3
"""large_value_perf_gate.py — deep-pipelined SET (read side) + GET (write side)
throughput vs vendored redis 7.2.4 across value sizes, to track the large-value
framing gap in BOTH directions.

Profiling evidence for frankenredis-largeval-bigbulk-zerocopy-qesp3: fr is FASTER
than redis on small/medium values (per-op machinery wins) but its throughput
PLATEAUS at ~2 GB/s for large values while redis scales to ~4.5 GB/s — because
fr copies the value twice on the SET write path (socket -> per-conn read_buf ->
Value::String) whereas redis reads a large bulk directly into the argument object
(networking.c processMultibulkBuffer "big argument" optimization, 1 copy). The
crossover is between 64KB and 256KB.

Reports the fr/redis ops/s ratio per size. This is a PERF tracker, not a pass/fail
correctness gate: it prints the ratios and flags sizes below `--min-ratio`
(default 0.9) so progress on the big-bulk lever is visible.

CRITICAL MEASUREMENT CAVEAT: fr-server is single-threaded (one mio event loop).
A leftover replica/PSYNC connection (e.g. from an interrupted replication probe)
steals event-loop cycles AND forces fr to propagate every write to it, which
craters throughput and produces bogus sub-0.3x ratios across ALL commands. Always
confirm `INFO replication connected_slaves:0` on the fr port before trusting
numbers (this script checks and warns).

Usage: large_value_perf_gate.py <oracle_port> <fr_port> [--min-ratio 0.9]
"""
import socket, sys, time, re


def conn(port):
    s = socket.create_connection(("127.0.0.1", port), timeout=30)
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return s


def connected_slaves(port):
    s = conn(port)
    s.sendall(b"INFO replication\r\n")
    raw = b""
    while b"connected_slaves" not in raw:
        raw += s.recv(65536)
    s.close()
    return int(re.search(rb"connected_slaves:(\d+)", raw).group(1))


def bench_set(port, vsize, n, reps=5):
    s = conn(port)
    one = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$%d\r\n%s\r\n" % (vsize, b"x" * vsize)
    pipe = one * n
    want = n * 5  # each reply is exactly "+OK\r\n" (5 bytes); count bytes, not
    best = 1e9    # split-prone markers
    for _ in range(reps):
        t = time.perf_counter()
        s.sendall(pipe)
        got = 0
        while got < want:
            got += len(s.recv(1 << 20))
        best = min(best, time.perf_counter() - t)
    s.close()
    return n / best, (vsize * n) / best / 1e6  # ops/s, MB/s


def bench_get(port, vsize, n, reps=5):
    s = conn(port)
    # seed the key, then deep-pipeline GET. Each reply is
    # "$<vsize>\r\n<vsize bytes>\r\n" — count reply bytes (split-proof).
    s.sendall(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$%d\r\n%s\r\n" % (vsize, b"x" * vsize))
    while b"+OK" not in s.recv(100):
        pass
    one = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n"
    pipe = one * n
    per = len(b"$%d\r\n" % vsize) + vsize + 2
    want = per * n
    best = 1e9
    for _ in range(reps):
        t = time.perf_counter()
        s.sendall(pipe)
        got = 0
        while got < want:
            got += len(s.recv(1 << 20))
        best = min(best, time.perf_counter() - t)
    s.close()
    return n / best, (vsize * n) / best / 1e6


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    OR, FR = int(sys.argv[1]), int(sys.argv[2])
    min_ratio = 0.9
    if "--min-ratio" in sys.argv:
        min_ratio = float(sys.argv[sys.argv.index("--min-ratio") + 1])

    for label, port in (("fr", FR), ("redis", OR)):
        cs = connected_slaves(port)
        if cs:
            print(f"WARNING: {label} (port {port}) has connected_slaves={cs} — "
                  f"throughput numbers will be contaminated by replica propagation.")

    sizes = [(64, 30000), (1024, 20000), (4096, 15000), (16384, 8000),
             (65536, 3000), (262144, 1200), (1048576, 400)]
    below = []
    for op, fn in (("SET", bench_set), ("GET", bench_get)):
        print("=" * 78)
        for vs, n in sizes:
            fo, fm = fn(FR, vs, n)
            ro, rm = fn(OR, vs, n)
            ratio = fo / ro
            flag = "  <-- below min" if ratio < min_ratio else ""
            if ratio < min_ratio:
                below.append((op, vs, ratio))
            print(f"{op} val={vs:>8}B  fr={fo:>9.0f} op/s ({fm:6.0f} MB/s)  "
                  f"redis={ro:>9.0f} ({rm:6.0f} MB/s)  ratio={ratio:.2f}x{flag}")
    print("=" * 78)
    if below:
        # SET below-min == read-side big-bulk lever; GET below-min == write-side
        # scatter-gather lever (both in bead largeval-bigbulk-zerocopy-qesp3).
        print(f"{len(below)} (op,size) below {min_ratio}x (large-value framing gap, "
              f"bead largeval-bigbulk-zerocopy-qesp3): " +
              ", ".join(f"{op} {vs}B={r:.2f}x" for op, vs, r in below))
    else:
        print(f"all SET/GET sizes >= {min_ratio}x vs redis 7.2.4")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
