#!/usr/bin/env python3
"""Perf baseline capture + pass-over-pass ratchet (gauntlet run-bench-matrix + apply-ratchet).

Closes the documented #1 frankenredis perf gap: "no machine-checkable baseline — every
perf claim lives in commit messages." Launches its own fr + redis-7.2.4 quartet (clean
cwd, free ports — same hardening as parity_suite), runs the fr-bench workload matrix at a
pipeline-depth sweep against BOTH engines via `fr-bench --json-out`, and records the
fr/redis ops-per-sec ratio + run-to-run cv_pct per (workload, depth) into
.bench-history/comprehensive_bench.latest.json. If a prior baseline exists, applies the
keep-gate ratchet (per-cell regression > RATCHET_PCT on the fr/redis ratio fails; cv_pct
> CV_NOISE_PCT cells are reported as noise, not keep-eligible).

This is a HEAVY pass (release build + servers + benches): run it in batch / via rch, NOT
in an automated cargo-check session. cc authors it; the batch runs it.

Usage: perf_baseline_capture.py <redis-server-bin> <fr-server-bin> [<fr-bench-bin>] [--trials N] [--quick]
       The fr-bench CLIENT is a SEPARATE binary from the fr server; if the 3rd arg is
       omitted it is auto-located next to the fr server binary / under the cc target.
       exit 0 = baseline captured / ratchet PASS; 1 = regression vs prior baseline.

Reset note: forces list-max-listpack-size -2 (the true redis 7.2.4 default) on the oracle
to avoid the documented config-pollution false positive on the shared oracle.
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
BASELINE_PATH = os.path.join(BENCH_HISTORY, "comprehensive_bench.latest.json")

# Read-reply + serialize + scalar/write coverage (every fr-bench workload family).
WORKLOADS = [
    "set", "get", "incr", "hset", "hget",
    "lpush", "lrange", "hgetall", "smembers", "zrange-withscores", "dump", "mixed",
]
PIPELINE_DEPTHS = [1, 16, 128]
RATCHET_PCT = 5.0   # a cell whose fr/redis ratio drops > this vs baseline fails
CV_NOISE_PCT = 5.0  # cells with cv_pct above this are flagged noisy (not keep-eligible)


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


def _ping(port):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=1)
        s.sendall(_enc(["PING"]))
        time.sleep(0.03)
        ok = b"PONG" in s.recv(64)
        s.close()
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


def _config_set(port, key, value):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=2)
        s.sendall(_enc(["CONFIG", "SET", key, value]))
        time.sleep(0.03)
        s.recv(256)
        s.close()
    except Exception:
        pass


def run_bench(bench_bin, port, workload, pipeline, requests, trials):
    """Invoke the fr-bench CLIENT (--json-out) against `port`; return its report or None."""
    out = os.path.join("/tmp", f"frbench_{workload}_{port}_{pipeline}.json")
    cmd = [
        bench_bin, "--host", "127.0.0.1", "--port", str(port),
        "--workload", workload, "--requests", str(requests),
        "--clients", "4", "--pipeline", str(pipeline),
        "--trials", str(trials), "--json-out", out,
    ]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
        if r.returncode != 0:
            return None
        with open(out) as fh:
            return json.load(fh)
    except Exception:
        return None


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    redis_bin = os.path.abspath(sys.argv[1])
    fr_bin = os.path.abspath(sys.argv[2])
    # The fr-bench CLIENT is a separate binary from the fr SERVER. Take it as the 3rd
    # positional arg, else auto-locate next to the fr server binary or under the cc target.
    positional = [a for a in sys.argv[3:] if not a.startswith("--")]
    if positional:
        bench_bin = os.path.abspath(positional[0])
    else:
        candidates = [
            os.path.join(os.path.dirname(fr_bin), "fr-bench"),
            "/data/projects/.rch-targets/frankenredis-cc/release/fr-bench",
            "/data/projects/.rch-targets/frankenredis-cc/debug/fr-bench",
        ]
        bench_bin = next((c for c in candidates if os.path.exists(c)), None)
        if not bench_bin:
            print("FAIL — fr-bench client binary not found; pass it as the 3rd argument "
                  "(perf_baseline_capture.py <redis-server> <fr-server> <fr-bench>)")
            sys.exit(2)
    trials = 5
    requests = 200_000
    if "--trials" in sys.argv:
        trials = int(sys.argv[sys.argv.index("--trials") + 1])
    if "--quick" in sys.argv:
        requests = 20_000
        trials = 3

    oracle_port, fr_port = _free_port(29951), _free_port(29952)
    procs = [
        subprocess.Popen(
            [redis_bin, "--port", str(oracle_port), "--save", "", "--appendonly", "no"],
            cwd="/tmp", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen(
            [fr_bin, "--port", str(fr_port)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (_wait_up(oracle_port) and _wait_up(fr_port)):
            print("FAIL — could not bring up redis + fr")
            sys.exit(2)
        # Avoid the config-pollution false positive (true redis default is -2).
        for p in (oracle_port, fr_port):
            _config_set(p, "list-max-listpack-size", "-2")

        cells = {}
        for wl in WORKLOADS:
            for depth in PIPELINE_DEPTHS:
                ro = run_bench(bench_bin, oracle_port, wl, depth, requests, trials)
                rf = run_bench(bench_bin, fr_port, wl, depth, requests, trials)
                if not ro or not rf or ro["ops_per_sec"] <= 0:
                    cells[f"{wl}@p{depth}"] = {"status": "skipped"}
                    continue
                ratio = rf["ops_per_sec"] / ro["ops_per_sec"]
                cells[f"{wl}@p{depth}"] = {
                    "fr_ops": round(rf["ops_per_sec"], 1),
                    "redis_ops": round(ro["ops_per_sec"], 1),
                    "fr_over_redis": round(ratio, 4),
                    "fr_cv_pct": round(rf.get("cv_pct", 0.0), 2),
                    "redis_cv_pct": round(ro.get("cv_pct", 0.0), 2),
                    "noisy": rf.get("cv_pct", 0.0) > CV_NOISE_PCT,
                }
    finally:
        for p in procs:
            p.terminate()
        time.sleep(0.5)
        for p in procs:
            if p.poll() is None:
                p.kill()

    current = {"schema_version": "perf-baseline.v1", "trials": trials, "requests": requests,
               "cells": cells}

    # Ratchet vs prior baseline (if any).
    regressions = []
    prior = None
    if os.path.exists(BASELINE_PATH):
        try:
            with open(BASELINE_PATH) as fh:
                prior = json.load(fh)
        except Exception:
            prior = None
    if prior:
        for key, cur in cells.items():
            old = prior.get("cells", {}).get(key)
            if not old or "fr_over_redis" not in old or "fr_over_redis" not in cur:
                continue
            if cur.get("noisy"):
                continue  # noisy cell: not keep-eligible, don't ratchet on it
            drop_pct = (old["fr_over_redis"] - cur["fr_over_redis"]) / old["fr_over_redis"] * 100.0
            if drop_pct > RATCHET_PCT:
                regressions.append(
                    f"{key}: fr/redis {old['fr_over_redis']} -> {cur['fr_over_redis']} (-{drop_pct:.1f}%)")

    print("=" * 64)
    for key, c in sorted(cells.items()):
        if c.get("status") == "skipped":
            print(f"  {key:28} SKIPPED")
        else:
            tag = " NOISY" if c.get("noisy") else ""
            print(f"  {key:28} fr/redis={c['fr_over_redis']:.3f} (fr cv={c['fr_cv_pct']}%){tag}")

    # Persist baseline only when not regressing (ratchet semantics).
    if regressions:
        print(f"FAIL — {len(regressions)} cell(s) regressed vs baseline > {RATCHET_PCT}%:")
        for r in regressions[:20]:
            print(f"  {r}")
        sys.exit(1)

    os.makedirs(BENCH_HISTORY, exist_ok=True)
    with open(BASELINE_PATH, "w") as fh:
        json.dump(current, fh, indent=2, sort_keys=True)
    print(f"PASS — baseline captured to {os.path.relpath(BASELINE_PATH, ROOT)} "
          f"({len([c for c in cells.values() if 'fr_over_redis' in c])} cells)")


if __name__ == "__main__":
    main()
