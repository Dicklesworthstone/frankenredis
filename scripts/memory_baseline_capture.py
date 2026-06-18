#!/usr/bin/env python3
"""Memory-domination baseline: fresh-process RSS per data-type, fr vs redis 7.2.4.

The throughput matrix (perf_baseline_capture.py) does not measure RAM — yet RAM is where
fr loses most vs redis (keyspace dict ~5.4x = uhthd, zset ~1.54x = uybhq). This captures
the RAM-domination scorecard: for each data-type dataset it starts a FRESH fr + redis,
loads a fixed dataset, lets it settle, and samples both used_memory (fr's modeled estimate
— usually near parity) AND used_memory_rss (the REAL process RAM — the domination metric).

Fresh-process per type is deliberate: RSS does NOT shrink after FLUSHALL (the allocator
retains freed pages), so a flush-then-load understates the gap. Each type gets its own
clean server pair. (Per the long-standing "measure fresh-process RSS, not used_memory"
lesson.)

Emits .bench-history/memory_baseline.latest.json + a ratchet (an RSS ratio that worsens
> RATCHET_PCT vs the prior baseline fails). RSS is noisy — treat single-run ratios as
indicative; the ratchet uses a generous band.

Usage: memory_baseline_capture.py <redis-server-bin> <fr-release-bin> [--quick]
       exit 0 = captured / ratchet PASS; 1 = RAM regression vs prior baseline.
HEAVY pass (starts many server pairs): run in batch / via rch, not in cargo-check.
"""
import json
import os
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BENCH_HISTORY = os.path.join(ROOT, ".bench-history")
BASELINE_PATH = os.path.join(BENCH_HISTORY, "memory_baseline.latest.json")
RATCHET_PCT = 15.0  # RSS is noisy; a generous band before flagging a regression


def _free_port(preferred):
    for port in range(preferred, preferred + 400):
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        except OSError:
            return port
    return preferred


def _enc(a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o


class Client:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=10)

    def cmd(self, *a):
        self.s.sendall(_enc(a))
        time.sleep(0.01)
        return self.s.recv(1 << 20)

    def pipe(self, cmds):
        buf = b"".join(_enc(c) for c in cmds)
        self.s.sendall(buf)
        # drain roughly one reply per command
        time.sleep(0.05)
        got = b""
        self.s.settimeout(5)
        try:
            while got.count(b"\r\n") < len(cmds):
                chunk = self.s.recv(1 << 20)
                if not chunk:
                    break
                got += chunk
        except Exception:
            pass

    def info_mem(self):
        r = self.cmd("INFO", "memory").decode(errors="replace")
        out = {}
        for line in r.splitlines():
            if line.startswith("used_memory:"):
                out["used_memory"] = int(line.split(":")[1])
            elif line.startswith("used_memory_rss:"):
                out["used_memory_rss"] = int(line.split(":")[1])
        return out


def _ping(port):
    try:
        c = Client(port)
        ok = b"PONG" in c.cmd("PING")
        return ok
    except Exception:
        return False


def _wait_up(port, deadline=10):
    t0 = time.time()
    while time.time() - t0 < deadline:
        if _ping(port):
            return True
        time.sleep(0.1)
    return False


def load_dataset(port, kind, scale):
    c = Client(port)
    c.cmd("FLUSHALL")
    if kind == "keyspace":  # many tiny string keys -> the dict RAM gap (uhthd)
        batch = [("SET", f"k:{i}", "v") for i in range(scale)]
    elif kind == "string_1k":
        val = "x" * 1024
        batch = [("SET", f"s:{i}", val) for i in range(scale // 8)]
    elif kind == "list":
        batch = [("RPUSH", f"l:{i}", *[str(j) for j in range(64)]) for i in range(scale // 64)]
    elif kind == "hash":
        batch = [("HSET", f"h:{i}", *sum(([f"f{j}", str(j)] for j in range(32)), []))
                 for i in range(scale // 32)]
    elif kind == "set":
        batch = [("SADD", f"st:{i}", *[f"m{j}" for j in range(32)]) for i in range(scale // 32)]
    elif kind == "zset":  # the zset RAM gap (uybhq)
        batch = [("ZADD", f"z:{i}", *sum(([str(j), f"m{j}"] for j in range(32)), []))
                 for i in range(scale // 32)]
    elif kind == "stream":
        batch = [("XADD", f"x:{i}", "*", "f", "v") for i in range(scale)]
    else:
        batch = []
    # send in chunks to bound memory of the request buffer
    for off in range(0, len(batch), 500):
        c.pipe(batch[off:off + 500])
    time.sleep(1.0)  # settle


def measure_type(redis_bin, fr_bin, kind, scale):
    op, fp = _free_port(29951), _free_port(29952)
    procs = [
        subprocess.Popen([redis_bin, "--port", str(op), "--save", "", "--appendonly", "no"],
                         cwd="/tmp", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen([fr_bin, "--port", str(fp)],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (_wait_up(op) and _wait_up(fp)):
            return None
        load_dataset(op, kind, scale)
        load_dataset(fp, kind, scale)
        rm, fm = Client(op).info_mem(), Client(fp).info_mem()
        if not rm.get("used_memory_rss") or not fm.get("used_memory_rss"):
            return None
        return {
            "redis_rss": rm["used_memory_rss"],
            "fr_rss": fm["used_memory_rss"],
            "rss_ratio": round(fm["used_memory_rss"] / rm["used_memory_rss"], 3),
            "redis_used": rm.get("used_memory"),
            "fr_used": fm.get("used_memory"),
            "used_ratio": round(fm.get("used_memory", 0) / max(rm.get("used_memory", 1), 1), 3),
        }
    finally:
        for p in procs:
            p.terminate()
        time.sleep(0.4)
        for p in procs:
            if p.poll() is None:
                p.kill()


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    redis_bin, fr_bin = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
    scale = 20_000 if "--quick" in sys.argv else 200_000

    types = ["keyspace", "string_1k", "list", "hash", "set", "zset", "stream"]
    cells = {}
    for kind in types:
        res = measure_type(redis_bin, fr_bin, kind, scale)
        cells[kind] = res if res else {"status": "skipped"}

    current = {"schema_version": "memory-baseline.v1", "scale": scale, "cells": cells}

    prior = None
    if os.path.exists(BASELINE_PATH):
        try:
            prior = json.load(open(BASELINE_PATH))
        except Exception:
            prior = None
    regressions = []
    if prior:
        for kind, cur in cells.items():
            old = prior.get("cells", {}).get(kind)
            if not old or "rss_ratio" not in old or "rss_ratio" not in cur:
                continue
            worsened = (cur["rss_ratio"] - old["rss_ratio"]) / old["rss_ratio"] * 100.0
            if worsened > RATCHET_PCT:
                regressions.append(
                    f"{kind}: RSS ratio {old['rss_ratio']} -> {cur['rss_ratio']} (+{worsened:.1f}% worse)")

    print("=" * 64)
    print("  data-type        fr/redis RSS    fr/redis used_memory")
    for kind, c in cells.items():
        if c.get("status") == "skipped":
            print(f"  {kind:15} SKIPPED")
        else:
            print(f"  {kind:15} {c['rss_ratio']:>8.3f}x      {c['used_ratio']:>8.3f}x")

    if regressions:
        print(f"FAIL — {len(regressions)} data-type(s) RAM-regressed vs baseline > {RATCHET_PCT}%:")
        for r in regressions:
            print(f"  {r}")
        sys.exit(1)

    os.makedirs(BENCH_HISTORY, exist_ok=True)
    json.dump(current, open(BASELINE_PATH, "w"), indent=2, sort_keys=True)
    print(f"PASS — memory baseline captured to {os.path.relpath(BASELINE_PATH, ROOT)}")


if __name__ == "__main__":
    main()
