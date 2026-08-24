#!/usr/bin/env python3
"""Differential maxmemory-eviction behaviour: FrankenRedis vs live Redis 7.2.4.

    python3 scripts/maxmemory_eviction_differ.py --fr /tmp/fr_bin \
        --redis legacy_redis_code/redis/src/redis-server

WHAT THIS CATCHES (frankenredis-uhthd). Under an `allkeys-*` policy Redis evicts
until it is back under `maxmemory` and KEEPS SERVING; it only returns `-OOM` when it
genuinely cannot free anything. fr used to give up after four evictions per command
and refuse the write: measured at `--maxmemory 100mb --maxmemory-policy allkeys-lru`
over a 1M SET load, Redis evicted 143,334 keys and never errored while fr evicted
450 and then answered

    -OOM command not allowed when used memory > 'maxmemory'.

An allkeys-lru server that refuses writes is a straight defect, and no unit test
catches it because it only appears once the keyspace is genuinely over the limit
with a real command stream behind it.

THE LOAD GOES THROUGH THE NORMAL COMMAND PATH, NOT `DEBUG POPULATE`. The pre-command
eviction check is what is under test, and `DEBUG POPULATE` bypasses it -- an earlier
version of this measurement did exactly that and saw nothing.

THE COMPARISON IS DIFFERENTIAL. fr is required to refuse no more writes than Redis
refuses on the identical schedule, rather than to refuse zero absolutely: Redis may
legitimately return `-OOM` if it cannot keep up, and pinning an absolute zero would
be asserting something upstream does not promise.

TWO METRICS, BOTH REPORTED. `used_memory` is each engine's own accounting; VmRSS is
what the operator's machine actually spends. They diverge -- Redis tracks real
allocation so its RSS lands near its limit, while fr's is a logical model that omits
index overhead -- so the RSS overshoot ratio is reported for both engines and is the
honest number for "did maxmemory bound anything".

Exit 0 = fr matched Redis. Exit 1 = a real divergence.
"""

import argparse
import hashlib
import platform
import socket
import subprocess
import sys
import tempfile
import time

OOM_PREFIX = b"-OOM"


def enc(*parts):
    out = b"*%d\r\n" % len(parts)
    for p in parts:
        b = p.encode() if isinstance(p, str) else p
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


def free_port(start, taken):
    for port in range(start, start + 500):
        if port in taken:
            continue
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        except OSError:
            taken.add(port)
            return port
    raise SystemExit("no free port")


def wait_ready(port, proc, timeout=180):
    for _ in range(timeout * 10):
        if proc.poll() is not None:
            raise SystemExit(f"server exited early rc={proc.returncode}")
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.3).close()
            return
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"server on {port} never accepted")


def vmrss_bytes(pid):
    with open(f"/proc/{pid}/status", "r", encoding="utf-8") as fh:
        for line in fh:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    raise SystemExit(f"no VmRSS for pid {pid}")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def run_arm(label, binary, port, maxmemory, keys, value_len):
    workdir = tempfile.mkdtemp(prefix=f"evictdiff_{label}_")
    proc = subprocess.Popen(
        [
            binary, "--port", str(port),
            "--dir", workdir,
            "--dbfilename", "evictdiff-nonexistent.rdb",
            "--save", "", "--appendonly", "no",
            "--maxmemory", str(maxmemory),
            "--maxmemory-policy", "allkeys-lru",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    wait_ready(port, proc)
    sock = socket.create_connection(("127.0.0.1", port), timeout=300)
    sock.settimeout(300)

    value = b"v" * value_len
    refused = 0
    chunk = 2000
    for base in range(0, keys, chunk):
        need = min(chunk, keys - base)
        sock.sendall(
            b"".join(enc("SET", f"memkey:{i}", value) for i in range(base, base + need))
        )
        acc = b""
        while acc.count(b"\r\n") < need:
            got = sock.recv(1 << 20)
            if not got:
                raise SystemExit(f"{label}: connection closed at {base}")
            acc += got
        refused += sum(1 for line in acc.split(b"\r\n") if line.startswith(OOM_PREFIX))

    rss = vmrss_bytes(proc.pid)

    def query(*parts):
        sock.sendall(enc(*parts))
        time.sleep(0.2)
        return sock.recv(1 << 20)

    dbsize = int(query("DBSIZE").decode().strip().lstrip(":"))

    def info_field(section, field):
        sock.sendall(enc("INFO", section))
        time.sleep(0.25)
        blob = sock.recv(1 << 22).decode("utf-8", "replace")
        for line in blob.splitlines():
            if line.startswith(field + ":"):
                return int(line.split(":", 1)[1].strip())
        return -1

    row = {
        "refused": refused,
        "dbsize": dbsize,
        "evicted_keys": info_field("stats", "evicted_keys"),
        "used_memory": info_field("memory", "used_memory"),
        "rss": rss,
        "rss_over_limit": rss / maxmemory,
    }
    proc.kill()
    proc.wait(timeout=15)
    return row


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fr", required=True)
    ap.add_argument("--redis", required=True)
    ap.add_argument("--maxmemory", type=int, default=100 * 1024 * 1024)
    ap.add_argument("--keys", type=int, default=1_000_000)
    ap.add_argument("--value-len", type=int, default=48)
    args = ap.parse_args()

    taken = set()
    # fr_b is the A/A null partner: same binary, same schedule, own process.
    arms = [
        ("fr_a", args.fr),
        ("fr_b", args.fr),
        ("redis", args.redis),
    ]
    rows = {}
    for label, binary in arms:
        rows[label] = run_arm(
            label, binary, free_port(7960, taken), args.maxmemory, args.keys, args.value_len
        )

    print(f"maxmemory = {args.maxmemory / 1e6:.1f} MB, policy allkeys-lru, {args.keys} SETs\n")
    header = f"  {'arm':6s} {'refused':>9s} {'dbsize':>10s} {'evicted':>10s} {'used_mem MB':>12s} {'RSS MB':>9s} {'RSS/limit':>10s}"
    print(header)
    for label in ("fr_a", "fr_b", "redis"):
        r = rows[label]
        print(
            f"  {label:6s} {r['refused']:9d} {r['dbsize']:10d} {r['evicted_keys']:10d} "
            f"{r['used_memory'] / 1e6:12.1f} {r['rss'] / 1e6:9.1f} {r['rss_over_limit']:10.2f}x"
        )

    print("\n=== provenance ===")
    print(f"  host          {platform.node()}")
    print(f"  fr    sha256  {sha256_file(args.fr)}")
    print(f"  redis sha256  {sha256_file(args.redis)}")
    print(
        "  redis version "
        + subprocess.run([args.redis, "--version"], capture_output=True, text=True).stdout.strip()
    )

    failures = []
    # Anti-vacuity: if nothing was ever evicted, the limit was never reached and the
    # run proves nothing about eviction either way.
    if rows["redis"]["evicted_keys"] <= 0:
        print("\n  NOT PROVEN: redis evicted nothing; the load never reached maxmemory")
        return 1
    if rows["fr_a"]["evicted_keys"] <= 0:
        failures.append("fr evicted nothing while redis evicted "
                        f"{rows['redis']['evicted_keys']}")

    # The defect under test: refusing writes upstream would have served.
    if rows["fr_a"]["refused"] > rows["redis"]["refused"]:
        failures.append(
            f"fr refused {rows['fr_a']['refused']} writes vs redis {rows['redis']['refused']} "
            "under allkeys-lru"
        )

    # A/A null on the refusal count and the keyspace size.
    if rows["fr_a"]["refused"] != rows["fr_b"]["refused"]:
        failures.append(
            f"A/A null broken: fr_a refused {rows['fr_a']['refused']}, "
            f"fr_b refused {rows['fr_b']['refused']}"
        )

    print("\n=== verdict ===")
    if failures:
        for f in failures:
            print(f"  FAIL  {f}")
        return 1
    print(
        f"  PASS: fr refused {rows['fr_a']['refused']} writes vs redis "
        f"{rows['redis']['refused']}; fr RSS {rows['fr_a']['rss_over_limit']:.2f}x the limit "
        f"vs redis {rows['redis']['rss_over_limit']:.2f}x"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
