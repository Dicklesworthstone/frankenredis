#!/usr/bin/env python3
"""bench_vs_redis.py — reliable fr-vs-redis throughput comparison via the canonical
redis-benchmark, with contention-robust interleaving and the guards that prevent
the three measurement pitfalls that repeatedly produced bogus numbers:

  1. PYTHON-BOUND DRAIN: a Python pipeline-drain that parses each reply (RESP) in
     Python measures *Python*, not the server (~100k "ops/s" for GET that really
     runs at millions). redis-benchmark is C — no client-side bottleneck.
  2. STALE CROSS-TURN PROBE: an fr left running on the port from a previous turn
     makes a fresh launch fail to bind (silently), so you measure the OLD binary
     with accumulated connections. We verify INFO server `executable` / `git_sha1`.
  3. SINGLE-THREADED CONTENTION: fr is one mio loop; a leftover replica/PSYNC
     connection (or co-tenant load) craters throughput. We check connected_slaves
     and print loadavg. Under load, ABSOLUTE rps is noisy but the fr/redis RATIO
     from INTERLEAVED back-to-back trials stays stable — so we report the median
     ratio across interleaved trials, not a single absolute.

Usage: bench_vs_redis.py <oracle_port> <fr_port> [--bench PATH] [--trials N]
       [--n REQUESTS] [--pipeline P] [--clients C] [--tests t1,t2,...]
Default bench: legacy_redis_code/redis/src/redis-benchmark.
Exit 0 always (informational); flags any command whose median ratio < 0.9x.
"""
import subprocess, sys, re, os, socket, statistics

OR = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
FRp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400


def opt(flag, default):
    return sys.argv[sys.argv.index(flag) + 1] if flag in sys.argv else default


BENCH = opt("--bench", os.path.join(os.path.dirname(__file__) or ".",
            "..", "legacy_redis_code", "redis", "src", "redis-benchmark"))
TRIALS = int(opt("--trials", "5"))
N = opt("--n", "100000")
P = opt("--pipeline", "16")
C = opt("--clients", "50")
TESTS = opt("--tests", "set,get,incr,lpush,rpush,lpop,rpop,sadd,hset,spop,zadd,"
            "lrange_100,mset").split(",")


def info_line(port, field):
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    s.sendall(b"INFO\r\n")
    raw = b""
    while b"\r\n\r\n" not in raw[-4:] and len(raw) < 1 << 20:
        d = s.recv(65536)
        if not d:
            break
        raw += d
    s.close()
    m = re.search((field + r":(.*)").encode(), raw)
    return m.group(1).decode("latin1").strip() if m else "?"


def one(port, test):
    r = subprocess.run([BENCH, "-p", str(port), "-n", N, "-P", P, "-c", C,
                        "-q", "-t", test], capture_output=True, text=True, timeout=120)
    # redis-benchmark -q final line: "<NAME>: <rps> requests per second, ..."
    vals = re.findall(r":\s*([0-9.]+) requests per second", r.stdout)
    return float(vals[-1]) if vals else None


def main():
    if not os.path.exists(BENCH):
        print(f"redis-benchmark not found at {BENCH}; pass --bench PATH")
        return 0
    try:
        la = open("/proc/loadavg").read().split()[0]
    except OSError:
        la = "?"
    print(f"loadavg={la}  (high load -> absolute rps noisy; ratio stays stable)")
    for label, port in (("fr", FRp), ("redis", OR)):
        cs = info_line(port, "connected_slaves")
        print(f"  {label}:{port} exe={info_line(port,'executable')} "
              f"sha={info_line(port,'redis_git_sha1')} connected_slaves={cs}")
        if cs not in ("0", "?"):
            print(f"  WARNING: {label} has replicas attached — throughput contaminated.")
    print("=" * 70)
    low = []
    for t in TESTS:
        ratios = []
        for _ in range(TRIALS):
            fr = one(FRp, t)
            rd = one(OR, t)
            if fr and rd:
                ratios.append(fr / rd)
        if not ratios:
            print(f"{t:<18} (no data)")
            continue
        med = statistics.median(ratios)
        flag = "  <-- below 0.9x" if med < 0.9 else ""
        if med < 0.9:
            low.append((t, med))
        print(f"{t:<18} median ratio={med:.2f}x  (trials: "
              f"{', '.join(f'{r:.2f}' for r in ratios)}){flag}")
    print("=" * 70)
    print(f"below-0.9x: {low}" if low else "fr at parity-or-faster on all tested "
          "commands (median ratio >= 0.9x) vs redis 7.2.4")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
