#!/usr/bin/env python3
"""Perf baseline capture + pass-over-pass ratchet (gauntlet run-bench-matrix + apply-ratchet).

Closes the documented #1 frankenredis perf gap: "no machine-checkable baseline — every
perf claim lives in commit messages." Launches its own fr + redis-7.2.4 server trio (clean
cwd, free ports — same hardening as parity_suite), runs the fr-bench workload matrix at a
pipeline-depth sweep against BOTH engines via `fr-bench --json-out`, and records the
fr/redis ops-per-sec ratio per (workload, depth) into
.bench-history/comprehensive_bench.latest.json. Every cell runs two identical FrankenRedis
servers plus Redis in one position-balanced invocation. Verdicts use the bootstrap 95% CI
of the A/A median and an explicit 2x null margin; CV is recorded as provenance only.
If a prior baseline exists, the ratchet fails only when the current ratio CI clears both
that null margin and the configured regression threshold.

This is a HEAVY pass (release build + servers + benches): run it in batch / via rch, NOT
in an automated cargo-check session. cc authors it; the batch runs it.

Usage: perf_baseline_capture.py <redis-server-bin> <fr-server-bin> [<fr-bench-bin>] [--trials N] [--quick]
       perf_baseline_capture.py --self-test
       The fr-bench CLIENT is a SEPARATE binary from the fr server; if the 3rd arg is
       omitted it is auto-located next to the fr server binary / under the cc target.
       exit 0 = baseline captured / ratchet PASS; 1 = regression vs prior baseline.

Reset note: forces list-max-listpack-size -2 (the true redis 7.2.4 default) on the oracle
to avoid the documented config-pollution false positive on the shared oracle.
"""
import json
import hashlib
import os
import socket
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BENCH_HISTORY = os.path.join(ROOT, ".bench-history")
BASELINE_PATH = os.path.join(BENCH_HISTORY, "comprehensive_bench.latest.json")
TMPDIR = tempfile.gettempdir()

# Read-reply + serialize + scalar/write coverage (every fr-bench workload family).
WORKLOADS = [
    "set", "get", "integer-get", "incr", "hset", "hget",
    "lpush", "xadd-maxlen", "lrange", "hgetall", "smembers",
    "zrange-withscores", "dump", "mixed",
]
PIPELINE_DEPTHS = [1, 16, 128]
RATCHET_PCT = 5.0
BOOTSTRAP_RESAMPLES = 20_000
DECISION_MARGIN_MULTIPLIER = 2.0
_U64_MASK = (1 << 64) - 1


def _free_port(preferred, reserved=()):
    reserved = set(reserved)
    for port in range(preferred, preferred + 400):
        if port in reserved:
            continue
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        except OSError:
            return port
    return preferred


def _free_port_triplet(preferred):
    redis_port = _free_port(preferred)
    fr_port = _free_port(redis_port + 1, reserved={redis_port})
    fr_null_port = _free_port(fr_port + 1, reserved={redis_port, fr_port})
    return redis_port, fr_port, fr_null_port


def _sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _cv_pct(samples):
    if len(samples) < 2:
        return 0.0
    mean = statistics.fmean(samples)
    if mean == 0:
        return 0.0
    return statistics.stdev(samples) / mean * 100.0


def _bootstrap_median_ci(samples):
    """Deterministic percentile-bootstrap 95% CI for the sample median."""
    if not samples:
        raise ValueError("bootstrap median CI requires at least one sample")
    if len(samples) == 1:
        return samples[0], samples[0]

    state = 0x5EEDF00DCAFEBABE
    for sample in samples:
        state ^= int(abs(sample) * 1_000_000.0) & _U64_MASK
        state = ((state << 17) | (state >> 47)) & _U64_MASK

    medians = []
    for _ in range(BOOTSTRAP_RESAMPLES):
        resample = []
        for _ in samples:
            state ^= state >> 12
            state ^= (state << 25) & _U64_MASK
            state ^= state >> 27
            state &= _U64_MASK
            draw = (state * 0x2545F4914F6CDD1D) & _U64_MASK
            resample.append(samples[draw % len(samples)])
        medians.append(statistics.median(resample))
    medians.sort()
    low = medians[round((len(medians) - 1) * 0.025)]
    high = medians[round((len(medians) - 1) * 0.975)]
    return low, high


def _median_ci_verdict(null_ci, effect_ci, margin=DECISION_MARGIN_MULTIPLIER):
    """Classify a paired ratio without consulting CV.

    `effect_ci` is the bootstrap CI for FrankenRedis/Redis throughput. Missing
    A/A evidence is always blocked, even if a standalone CV happens to be tiny.
    """
    if null_ci is None:
        return {
            "verdict": "BLOCKED_NO_NULL",
            "gate_low": None,
            "gate_high": None,
        }
    null_low, null_high = null_ci
    if not null_low <= 1.0 <= null_high:
        return {
            "verdict": "BLOCKED_NULL_BIAS",
            "gate_low": None,
            "gate_high": None,
        }

    radius = max(1.0 - null_low, null_high - 1.0)
    gate_low = 1.0 - margin * radius
    gate_high = 1.0 + margin * radius
    effect_low, effect_high = effect_ci
    if effect_low > gate_high:
        verdict = "FR_WIN"
    elif effect_high < gate_low:
        verdict = "FR_LOSS"
    else:
        verdict = "INDETERMINATE"
    return {
        "verdict": verdict,
        "gate_low": gate_low,
        "gate_high": gate_high,
    }


def _enc(a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o


def _ping(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=1) as s:
            s.sendall(_enc(["PING"]))
            time.sleep(0.03)
            return b"PONG" in s.recv(64)
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
        with socket.create_connection(("127.0.0.1", port), timeout=2) as s:
            s.sendall(_enc(["CONFIG", "SET", key, value]))
            time.sleep(0.03)
            s.recv(256)
    except Exception:
        pass


def _checked_executable(path, label):
    resolved = os.path.realpath(os.path.abspath(path))
    if not os.path.isfile(resolved) or not os.access(resolved, os.X_OK):
        print(f"FAIL — {label} is not an executable file: {path}")
        sys.exit(2)
    return resolved


def run_bench(
        bench_bin, bench_sha256, port, workload, pipeline, requests, key_prefix):
    """Invoke the fr-bench CLIENT (--json-out) against `port`; return its report or None."""
    out = os.path.join(
        TMPDIR,
        f"frbench_{os.getpid()}_{workload}_{port}_{pipeline}.json",
    )
    cmd = [
        bench_bin, "--host", "127.0.0.1", "--port", str(port),
        "--workload", workload, "--requests", str(requests),
        "--clients", "4", "--pipeline", str(pipeline),
        "--trials", "1", "--key-prefix", key_prefix, "--json-out", out,
    ]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
        if r.returncode != 0:
            return None
        with open(out, encoding="utf-8") as fh:
            report = json.load(fh)
        if (
            report.get("schema_version") != "fr_bench_report/v2"
            or report.get("harness_elf_sha256") != bench_sha256
            or report.get("key_prefix") != key_prefix
        ):
            return None
        return report
    except Exception:
        return None


def run_interleaved_cell(
        bench_bin, bench_sha256, ports, workload, pipeline, requests, rounds):
    """Measure A/A and A/B in one position-balanced top-level invocation."""
    orders = [
        ("fr", "fr_null", "redis"),
        ("redis", "fr_null", "fr"),
        ("fr_null", "redis", "fr"),
        ("fr", "redis", "fr_null"),
        ("redis", "fr", "fr_null"),
        ("fr_null", "fr", "redis"),
    ]
    samples = {"fr": [], "fr_null": [], "redis": []}
    for round_index in range(rounds):
        reports = {}
        prefix = (
            f"fr:baseline:{os.getpid()}:{workload}:"
            f"p{pipeline}:r{round_index}"
        )
        for arm in orders[round_index % len(orders)]:
            report = run_bench(
                bench_bin,
                bench_sha256,
                ports[arm],
                workload,
                pipeline,
                requests,
                prefix,
            )
            if not report or report.get("ops_per_sec", 0.0) <= 0:
                return {"status": "skipped"}
            reports[arm] = report
        for arm, report in reports.items():
            samples[arm].append(report["ops_per_sec"])

    null_ratios = [
        null / primary
        for null, primary in zip(samples["fr_null"], samples["fr"])
    ]
    effect_ratios = [
        primary / redis
        for primary, redis in zip(samples["fr"], samples["redis"])
    ]
    null_ci = _bootstrap_median_ci(null_ratios)
    effect_ci = _bootstrap_median_ci(effect_ratios)
    decision = _median_ci_verdict(null_ci, effect_ci)
    return {
        "fr_ops": round(statistics.median(samples["fr"]), 1),
        "fr_null_ops": round(statistics.median(samples["fr_null"]), 1),
        "redis_ops": round(statistics.median(samples["redis"]), 1),
        "fr_over_redis": round(statistics.median(effect_ratios), 6),
        "null_median": round(statistics.median(null_ratios), 9),
        "null_ci95_low": round(null_ci[0], 9),
        "null_ci95_high": round(null_ci[1], 9),
        "effect_ci95_low": round(effect_ci[0], 9),
        "effect_ci95_high": round(effect_ci[1], 9),
        "decision_gate_low": (
            None if decision["gate_low"] is None
            else round(decision["gate_low"], 9)
        ),
        "decision_gate_high": (
            None if decision["gate_high"] is None
            else round(decision["gate_high"], 9)
        ),
        "verdict": decision["verdict"],
        "margin_multiplier": DECISION_MARGIN_MULTIPLIER,
        "bootstrap_resamples": BOOTSTRAP_RESAMPLES,
        "harness_elf_sha256": bench_sha256,
        "same_invocation_aa": True,
        "position_balanced": True,
        "cv_used_as_provenance_only": True,
        "fr_cv_pct": round(_cv_pct(samples["fr"]), 2),
        "fr_null_cv_pct": round(_cv_pct(samples["fr_null"]), 2),
        "redis_cv_pct": round(_cv_pct(samples["redis"]), 2),
        "null_cv_pct": round(_cv_pct(null_ratios), 2),
        "effect_cv_pct": round(_cv_pct(effect_ratios), 2),
        "fr_ops_samples": [round(value, 3) for value in samples["fr"]],
        "fr_null_ops_samples": [
            round(value, 3) for value in samples["fr_null"]
        ],
        "redis_ops_samples": [round(value, 3) for value in samples["redis"]],
        "null_ratio_samples": [round(value, 9) for value in null_ratios],
        "effect_ratio_samples": [round(value, 9) for value in effect_ratios],
    }


def _self_test():
    # A tiny standalone CV never licenses a verdict without an A/A null.
    low_cv_provenance = _cv_pct([1.200, 1.201, 1.199, 1.200])
    assert low_cv_provenance < 1.0
    no_null = _median_ci_verdict(None, (1.20, 1.30))
    assert no_null["verdict"] == "BLOCKED_NO_NULL"

    # CV is deliberately absent from the decision API. Even provenance-only
    # high-CV data is admissible when its candidate median CI clears the 2x
    # null-derived margin.
    high_cv_provenance = _cv_pct([0.60, 0.85, 1.35, 1.60])
    decisive = _median_ci_verdict((0.99, 1.01), (1.08, 1.22))
    assert high_cv_provenance > 5.0
    assert decisive["verdict"] == "FR_WIN"
    assert abs(decisive["gate_high"] - 1.02) < 1.0e-12

    biased = _median_ci_verdict((1.03, 1.05), (1.20, 1.30))
    assert biased["verdict"] == "BLOCKED_NULL_BIAS"

    low, high = _bootstrap_median_ci([0.99, 1.0, 1.0, 1.01, 1.02, 0.98])
    assert low <= 1.0 <= high
    print(
        "PASS — median-CI gate blocks CV-only rows, admits decisive high-CV "
        "rows, and rejects biased A/A controls"
    )


def main():
    if "--self-test" in sys.argv:
        _self_test()
        return
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    redis_bin = _checked_executable(sys.argv[1], "redis-server")
    fr_bin = _checked_executable(sys.argv[2], "frankenredis")
    # The fr-bench CLIENT is a separate binary from the fr SERVER. Take it as the 3rd
    # positional arg, else auto-locate next to the fr server binary or under the cc target.
    positional = []
    tail = sys.argv[3:]
    index = 0
    while index < len(tail):
        argument = tail[index]
        if argument == "--trials":
            if index + 1 >= len(tail):
                print("FAIL — --trials requires a value")
                sys.exit(2)
            index += 2
            continue
        if argument == "--quick":
            index += 1
            continue
        if argument.startswith("--"):
            print(f"FAIL — unknown option: {argument}")
            sys.exit(2)
        positional.append(argument)
        index += 1
    if len(positional) > 1:
        print("FAIL — expected at most one fr-bench binary")
        sys.exit(2)
    if positional:
        bench_bin = _checked_executable(positional[0], "fr-bench")
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
        bench_bin = _checked_executable(bench_bin, "fr-bench")
    trials = 6
    requests = 200_000
    if "--trials" in sys.argv:
        trials = int(sys.argv[sys.argv.index("--trials") + 1])
    if "--quick" in sys.argv:
        requests = 20_000
        trials = 6
    if trials < 6:
        print("FAIL — --trials must be at least 6 for a complete position-balanced cycle")
        sys.exit(2)

    fr_sha256 = _sha256(fr_bin)
    redis_sha256 = _sha256(redis_bin)
    bench_sha256 = _sha256(bench_bin)
    print(
        "ELF_SHA256 "
        f"fr_a={fr_sha256} fr_b={fr_sha256} redis={redis_sha256} "
        f"fr_bench={bench_sha256}"
    )

    port_base = int(os.environ.get("FR_BENCH_PORT_BASE", "29951"))
    oracle_port, fr_port, fr_null_port = _free_port_triplet(port_base)
    procs = [
        subprocess.Popen(
            [redis_bin, "--port", str(oracle_port), "--save", "", "--appendonly", "no"],
            cwd=TMPDIR, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen(
            [fr_bin, "--port", str(fr_port)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen(
            [fr_bin, "--port", str(fr_null_port)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (
            _wait_up(oracle_port)
            and _wait_up(fr_port)
            and _wait_up(fr_null_port)
        ):
            print("FAIL — could not bring up redis + fr A/A pair")
            sys.exit(2)
        # Avoid the config-pollution false positive (true redis default is -2).
        for p in (oracle_port, fr_port, fr_null_port):
            _config_set(p, "list-max-listpack-size", "-2")

        ports = {
            "redis": oracle_port,
            "fr": fr_port,
            "fr_null": fr_null_port,
        }
        cells = {}
        for wl in WORKLOADS:
            for depth in PIPELINE_DEPTHS:
                cells[f"{wl}@p{depth}"] = run_interleaved_cell(
                    bench_bin,
                    bench_sha256,
                    ports,
                    wl,
                    depth,
                    requests,
                    trials,
                )
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=2)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait(timeout=2)

    current = {
        "schema_version": "perf-baseline.v2",
        "trials": trials,
        "requests": requests,
        "contract": {
            "same_invocation_aa": True,
            "position_balanced": True,
            "bootstrap_median_ci95": True,
            "margin_multiplier": DECISION_MARGIN_MULTIPLIER,
            "cv_used_as_provenance_only": True,
        },
        "elf_sha256": {
            "fr_a": fr_sha256,
            "fr_b": fr_sha256,
            "redis": redis_sha256,
            "fr_bench": bench_sha256,
        },
        "cells": cells,
    }

    # Ratchet vs prior baseline (if any).
    regressions = []
    prior = None
    if os.path.exists(BASELINE_PATH):
        try:
            with open(BASELINE_PATH, encoding="utf-8") as fh:
                prior = json.load(fh)
        except Exception:
            prior = None
    prior_contract = (
        prior.get("contract", {})
        if isinstance(prior, dict)
        else {}
    )
    prior_same_invocation_aa = prior_contract.get("same_invocation_aa")
    prior_is_admissible = bool(
        isinstance(prior, dict)
        and prior.get("schema_version") == "perf-baseline.v2"
        and isinstance(prior_same_invocation_aa, bool)
        and prior_same_invocation_aa
    )
    current["ratchet_prior_status"] = (
        "admissible_v2" if prior_is_admissible
        else "ignored_missing_or_legacy_without_same_invocation_aa"
    )
    if prior_is_admissible:
        for key, cur in cells.items():
            old = prior.get("cells", {}).get(key)
            if not old or "fr_over_redis" not in old or "fr_over_redis" not in cur:
                continue
            if cur.get("verdict", "").startswith("BLOCKED"):
                continue
            threshold = old["fr_over_redis"] * (1.0 - RATCHET_PCT / 100.0)
            null_radius = max(
                1.0 - cur["null_ci95_low"],
                cur["null_ci95_high"] - 1.0,
            )
            normalized_ci_high = cur["effect_ci95_high"] / old["fr_over_redis"]
            null_margin_floor = 1.0 - DECISION_MARGIN_MULTIPLIER * null_radius
            if (
                cur["effect_ci95_high"] < threshold
                and normalized_ci_high < null_margin_floor
            ):
                drop_pct = (
                    (old["fr_over_redis"] - cur["fr_over_redis"])
                    / old["fr_over_redis"]
                    * 100.0
                )
                regressions.append(
                    f"{key}: fr/redis {old['fr_over_redis']} -> "
                    f"{cur['fr_over_redis']} (-{drop_pct:.1f}%), current CI high "
                    f"{cur['effect_ci95_high']} < threshold {threshold:.6f}, "
                    f"normalized CI high {normalized_ci_high:.6f} < 2x-null "
                    f"floor {null_margin_floor:.6f}"
                )
    current["ratchet_regressions"] = regressions

    print("=" * 64)
    for key, c in sorted(cells.items()):
        if c.get("status") == "skipped":
            print(f"  {key:28} SKIPPED")
        else:
            print(
                f"  {key:28} fr/redis={c['fr_over_redis']:.3f} "
                f"CI=[{c['effect_ci95_low']:.3f},{c['effect_ci95_high']:.3f}] "
                f"null=[{c['null_ci95_low']:.3f},{c['null_ci95_high']:.3f}] "
                f"{c['verdict']} (effect cv={c['effect_cv_pct']}%, provenance only)"
            )

    os.makedirs(BENCH_HISTORY, exist_ok=True)
    with open(BASELINE_PATH, "w", encoding="utf-8") as fh:
        json.dump(current, fh, indent=2, sort_keys=True)
    if regressions:
        print(f"FAIL — {len(regressions)} cell(s) regressed vs baseline > {RATCHET_PCT}%:")
        for r in regressions[:20]:
            print(f"  {r}")
        print(f"Current throughput baseline still captured to {os.path.relpath(BASELINE_PATH, ROOT)}")
        sys.exit(1)

    print(f"PASS — baseline captured to {os.path.relpath(BASELINE_PATH, ROOT)} "
          f"({len([c for c in cells.values() if 'fr_over_redis' in c])} cells)")


if __name__ == "__main__":
    main()
