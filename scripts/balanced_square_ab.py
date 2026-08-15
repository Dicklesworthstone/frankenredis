#!/usr/bin/env python3
"""Balanced-square vs-incumbent A/B for FrankenRedis, usable on a CONTENDED host.

WHY THIS EXISTS
---------------
Every vs-incumbent harness in this repo gates on the host being quiet, and on a
64-way box shared by tens of agents that gate cannot be met. Measured here today:

  * `scripts/lua_eval_headtohead.sh` refused 21 of 21 invocations at an absolute
    loadavg ceiling; rescaling it per core (2b02caf16) helped, and it STILL
    refused 24 of 24 an hour later at load 27.9 against a 19.20 ceiling.
  * A four-arm throughput harness run at loadavg 58 produced A/A nulls of
    0.85-1.07 and a SAME-BINARY post/pre of 0.96-1.05 — two columns whose true
    value is exactly 1.0000. Recorded as INADMISSIBLE in
    `docs/perf_negative_evidence_ledger.md`.

The same wall was hit independently in franken_networkx, whose sanctioned
harness required five consecutive windows with EVERY cpu idle: its bead
`br-r37-c1-3s8x7` logged 25 consecutive attempts with zero admitted, and a run
aborted after 300 windows on one busy cpu. Its answer, committed as
`/data/projects/franken_networkx/scripts/balanced_square_ab.py` (72761094c), is
the design ported here. Three agents there hand-rolled it in scratchpads before
one committed it properly; this is a port of theirs, not a fourth hand-roll.

THE DESIGN. It does not try to make the host quiet. It makes the COMPARISON
immune to the host being busy:

  * Both arms run INSIDE one round, interleaved as a balanced square
    `A B B A A B B A`. Each arm occupies the same multiset of slot POSITIONS, so
    drift across a round — a peer's build starting, a cache warming, a governor
    step — hits both arms equally instead of biasing whichever went first.
  * Each arm carries its OWN A/A null: that arm's first-half slots divided by its
    second-half slots, which must come out 1.0. The square places the halves
    symmetrically, so a null that departs from 1.0 is drift or contention rather
    than slot position. Contention is therefore CAUGHT PER ROW, after the fact,
    instead of being excluded up front by a gate that can never pass.
  * A row whose null leaves [0.98, 1.02] is reported NULL-FAILED and its ratio is
    NOT a result. Refusing is the point.

This RELAXES NO EVIDENCE STANDARD. The incumbent is a live vendored
`redis-server` started in this same invocation; every arm's ELF SHA-256 is read
from `/proc/<pid>/exe` of the already-running process, so the harness cannot
compare a build against itself by accident; and provenance carries the OBSERVED
thread count, host, governor and runtime ISA. It replaces an unsatisfiable
precondition with a sound experimental design, nothing more.

USAGE
-----
    scripts/balanced_square_ab.py --fr-bin /tmp/fr_head --shapes cascade
    scripts/balanced_square_ab.py --fr-bin /tmp/fr_head --shapes eval --rounds 15

    --shapes     a registered shape set (see --list)
    --rounds     balanced squares per row (default 9)
    --ops        redis-benchmark operations per timed slot (default 50000)
    --pipeline   redis-benchmark -P depth (default 16)
    --expect-elf first 16 hex chars of the fr ELF you INTEND to measure; the run
                 aborts on mismatch, because pointing at a stale /tmp copy is the
                 cheapest way to publish a number about the wrong binary.

Ratio convention is fr_ops_per_sec / redis_ops_per_sec, so > 1 means FrankenRedis
is faster. That is the convention the ledger rows use.

ADDING A SHAPE SET. Append to SHAPE_SETS. A shape is
`(label, [seed commands], [benchmark argv])`. Every shape is error-probed on BOTH
engines before timing, because `redis-benchmark` counts an error reply as a
completed request and a refused command otherwise reads as enormous throughput.
Include at least one row the change under test CANNOT affect, as a control.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import platform
import random
import re
import shutil
import socket
import statistics
import subprocess
import sys
import time

SQUARE = "ABBAABBA"
NULL_BOUND = 0.02

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-benchmark")
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
CLI = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-cli")

# Shapes are grouped so a row set can be named on the command line rather than
# re-typed. The trailing control in each set is a command the work under test
# does not touch; a control that moves with the candidate means the row set is
# measuring the harness, not the change.
SHAPE_SETS: dict[str, list[tuple[str, list[str], list[str]]]] = {
    # The nine shapes front-classified onto the dispatch floor (frankenredis-ozrro).
    "cascade": [
        ("sintercard", ["SADD sc:a m1 m2 m3", "SADD sc:b m2 m3 m4"],
         ["SINTERCARD", "2", "sc:a", "sc:b"]),
        ("zrandmember", ["ZADD zz 1 a 2 b 3 c 4 d"], ["ZRANDMEMBER", "zz", "2"]),
        ("srandmember", ["SADD sbig m1 m2 m3 m4 m5 m6 m7 m8 m9 m10"],
         ["SRANDMEMBER", "sbig", "2"]),
        ("copy", ["SET kk vvvvvvvvvvvvvvvv"], ["COPY", "kk", "kdst"]),
        ("pttl", ["SET bb abcdefghijklmnop", "PEXPIRE bb 900000000"], ["PTTL", "bb"]),
        ("expiretime", ["SET kk vvvvvvvvvvvvvvvv", "EXPIREAT kk 4102444800"],
         ["EXPIRETIME", "kk"]),
        ("publish", [], ["PUBLISH", "ch", "hello"]),
        ("getbit", ["SET bb abcdefghijklmnop"], ["GETBIT", "bb", "5"]),
        ("geohash", ["GEOADD gg 13.361389 38.115556 Palermo"], ["GEOHASH", "gg", "Palermo"]),
        # Control: GET is not front-classified by that work.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # The standing Lua target: 50 redis.call('GET') per EVAL.
    "eval": [
        ("eval_50x_get", ["SET k val"],
         ["EVAL", "for i=1,50 do redis.call('GET', KEYS[1]) end return 1", "1", "k"]),
        ("get_control", ["SET k val"], ["GET", "k"]),
    ],
}


def sha256_of(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def running_image_sha(pid: int) -> str:
    """SHA-256 of the image the process is ACTUALLY executing.

    Hashing the path we intended to launch would not catch a stale copy, a
    symlink, or a harness that launched the same binary twice and called one of
    them the candidate. Reading `/proc/<pid>/exe` reports what is running.
    """
    return sha256_of(f"/proc/{pid}/exe")


def observed_threads(pid: int) -> int:
    """Threads the server ACTUALLY has, not the number any flag requested."""
    return len(os.listdir(f"/proc/{pid}/task"))


def provenance(fr_pid: int, redis_pid: int) -> dict:
    governor = "unknown"
    gov_path = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"
    if os.path.exists(gov_path):
        with open(gov_path) as handle:
            governor = handle.read().strip()
    isa = []
    with open("/proc/cpuinfo") as handle:
        flags = handle.read()
    for feature in ("avx512f", "avx2", "avx", "sse4_2"):
        if re.search(rf"\b{feature}\b", flags):
            isa.append(feature)
    with open("/proc/loadavg") as handle:
        loadavg = " ".join(handle.read().split()[:3])
    return {
        "host": socket.gethostname(),
        "kernel": platform.release(),
        "cores": os.cpu_count(),
        "governor": governor,
        "isa": isa[0] if isa else "unknown",
        "loadavg": loadavg,
        "fr_elf_sha256": running_image_sha(fr_pid),
        "redis_elf_sha256": running_image_sha(redis_pid),
        "fr_threads_observed": observed_threads(fr_pid),
        "redis_threads_observed": observed_threads(redis_pid),
    }


def wait_ready(port: int, timeout_s: float = 30.0) -> None:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        probe = subprocess.run([CLI, "-p", str(port), "ping"],
                               capture_output=True, text=True)
        if probe.returncode == 0 and "PONG" in probe.stdout:
            return
        time.sleep(0.2)
    raise SystemExit(f"server on port {port} never became ready")


def seed(port: int, commands: list[str]) -> None:
    for command in commands:
        subprocess.run([CLI, "-p", str(port)] + command.split(),
                       capture_output=True, text=True, check=False)


def error_probe(port: int, argv: list[str], engine: str, label: str) -> None:
    """A refused command reads as enormous throughput, so refuse to time it.

    `redis-benchmark` counts an error reply as a completed request. A shape that
    one engine rejects therefore produces a fast, confident, meaningless number.
    """
    out = subprocess.run([CLI, "-p", str(port)] + argv,
                         capture_output=True, text=True).stdout.strip()
    if out.startswith(("ERR", "WRONGTYPE", "NOPERM")) or "unknown command" in out:
        raise SystemExit(f"error probe failed: {engine} rejects `{label}`: {out}")


RPS = re.compile(r"([0-9]+\.[0-9]+) requests per second")


def time_slot(port: int, argv: list[str], ops: int, pipeline: int,
              client_core: str | None) -> float:
    """One timed slot: ops/s for a single redis-benchmark invocation."""
    cmd = [BENCH, "-p", str(port), "-n", str(ops), "-c", "1",
           "-P", str(pipeline), "-q"] + argv
    if client_core:
        cmd = ["taskset", "-c", client_core] + cmd
    out = subprocess.run(cmd, capture_output=True, text=True).stdout
    match = None
    for match in RPS.finditer(out):
        pass
    if match is None:
        raise SystemExit(f"redis-benchmark produced no rate for {argv}:\n{out}")
    return float(match.group(1))


def bootstrap_ci(values: list[float], iters: int = 2000,
                 seed_value: int = 20260814) -> tuple[float, float]:
    rng = random.Random(seed_value)
    n = len(values)
    medians = sorted(
        statistics.median(rng.choices(values, k=n)) for _ in range(iters)
    )
    return medians[int(0.025 * iters)], medians[int(0.975 * iters)]


def run_row(label: str, fr_port: int, redis_port: int, argv: list[str],
            rounds: int, ops: int, pipeline: int,
            client_core: str | None) -> dict:
    # Warm both arms once so neither pays first-touch inside a measured slot.
    time_slot(fr_port, argv, max(ops // 10, 1000), pipeline, client_core)
    time_slot(redis_port, argv, max(ops // 10, 1000), pipeline, client_core)

    ratios, null_redis, null_fr = [], [], []
    for _ in range(rounds):
        a_slots, b_slots = [], []
        for slot in SQUARE:
            if slot == "A":
                a_slots.append(time_slot(redis_port, argv, ops, pipeline, client_core))
            else:
                b_slots.append(time_slot(fr_port, argv, ops, pipeline, client_core))
        # ops/s, so fr/redis > 1 means fr is faster.
        ratios.append(statistics.median(b_slots) / statistics.median(a_slots))
        # Each arm's own first-half / second-half ratio. The square places the
        # halves symmetrically, so a departure from 1.0 is drift or contention,
        # not slot position.
        null_redis.append(statistics.median(a_slots[:2]) / statistics.median(a_slots[2:]))
        null_fr.append(statistics.median(b_slots[:2]) / statistics.median(b_slots[2:]))

    ratio = statistics.median(ratios)
    low, high = bootstrap_ci(ratios)
    n_redis, n_fr = statistics.median(null_redis), statistics.median(null_fr)
    nulls_ok = abs(n_redis - 1.0) <= NULL_BOUND and abs(n_fr - 1.0) <= NULL_BOUND
    if not nulls_ok:
        verdict = "NULL-FAILED"
    elif low <= 1.0 <= high:
        verdict = "STRADDLES-1"
    else:
        verdict = "ADMISSIBLE"
    return {
        "label": label,
        "ratio": ratio,
        "ci": (low, high),
        "null_redis": n_redis,
        "null_fr": n_fr,
        "verdict": verdict,
    }


def main(argv_in: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fr-bin", required=False)
    parser.add_argument("--shapes", default="cascade")
    parser.add_argument("--rounds", type=int, default=9)
    parser.add_argument("--ops", type=int, default=50000)
    parser.add_argument("--pipeline", type=int, default=16)
    parser.add_argument("--fr-port", type=int, default=27841)
    parser.add_argument("--redis-port", type=int, default=27842)
    parser.add_argument("--client-core", default=None)
    # (frankenredis-xvq1a) Optional server pinning. The per-arm nulls are each
    # arm's own first half over its second half — WITHIN-process drift — while the
    # reported ratio is a CROSS-process comparison, so placement between the two
    # server processes is a term the nulls structurally cannot see. Whether that
    # matters is WORKLOAD-DEPENDENT and was measured both ways on this host:
    #   * RESTORE decode (long ~40ms DEBUG-driven bursts, one connection): a
    #     cross-process A/A between two identical fr servers scattered 0.918-1.058
    #     over six invocations; pinning to symmetric core sets collapsed it to
    #     1.0106 [1.0015, 1.0159] and 1.0081 [0.9847, 1.0260].
    #   * THIS harness's redis-benchmark workload (-c1 -P16, many short ops):
    #     unpinned cross-process A/A came out 0.9974 and 1.0106, and PINNING DID
    #     NOT IMPROVE IT (1.0151 and 0.9896, one row null-failed). So the term is
    #     ~1% here and these flags buy nothing for the registered shape sets.
    # They are kept because the RESTORE result shows the term is real for other
    # workloads, and because a future shape set may be burst-shaped. Off by
    # default: do not pin without first showing --cross-null needs it.
    parser.add_argument("--fr-core", default=None,
                        help="taskset core list for the fr server (e.g. 0-3); "
                             "measured to buy nothing on the current shape sets")
    parser.add_argument("--redis-core", default=None,
                        help="taskset core list for the redis server; use a set "
                             "symmetric with --fr-core (same CCD, same size)")
    # Lets the harness MEASURE its own cross-process null instead of assuming it:
    # run the second arm as another fr, so the reported ratio should be 1.0. This
    # is the flag that settled the question above, and it is the one worth using.
    parser.add_argument("--cross-null", action="store_true",
                        help="replace the redis arm with a SECOND fr server; the "
                             "reported ratio is then a cross-process A/A and must "
                             "come out 1.0. Not a competitive row.")
    parser.add_argument("--expect-elf", default=None)
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args(argv_in)

    if args.list:
        for name, shapes in SHAPE_SETS.items():
            print(f"{name}: {', '.join(label for label, _, _ in shapes)}")
        return 0

    if not args.fr_bin:
        parser.error("--fr-bin is required")
    for path in (BENCH, REDIS, CLI, args.fr_bin):
        if not os.path.exists(path):
            raise SystemExit(f"missing binary: {path}")
    shapes = SHAPE_SETS.get(args.shapes)
    if shapes is None:
        raise SystemExit(f"unknown shape set {args.shapes}; try --list")

    def pinned(core: str | None, cmd: list[str]) -> list[str]:
        return (["taskset", "-c", core] + cmd) if core else cmd

    if (args.fr_core is None) != (args.redis_core is None):
        parser.error("--fr-core and --redis-core must be given together; pinning "
                     "one server and not the other is worse than pinning neither")

    fr = subprocess.Popen(
        pinned(args.fr_core, [args.fr_bin, "--port", str(args.fr_port)]),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    second_arm = ([args.fr_bin, "--port", str(args.redis_port)] if args.cross_null
                  else [REDIS, "--port", str(args.redis_port),
                        "--save", "", "--appendonly", "no"])
    redis = subprocess.Popen(pinned(args.redis_core, second_arm),
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_ready(args.fr_port)
        wait_ready(args.redis_port)
        prov = provenance(fr.pid, redis.pid)
        if args.expect_elf and not prov["fr_elf_sha256"].startswith(args.expect_elf):
            raise SystemExit(
                f"ELF mismatch: running image is {prov['fr_elf_sha256'][:16]}, "
                f"expected {args.expect_elf}")

        print("== provenance (self-reported from inside the running processes) ==")
        for key, value in prov.items():
            print(f"  {key:24} {value}")
        if args.cross_null:
            print("\n== CROSS-PROCESS A/A MODE: the second arm is another fr, so "
                  "every ratio below must be 1.0. NOT a competitive row. ==")
        if args.fr_core:
            print(f"  servers pinned: fr={args.fr_core} redis={args.redis_core}")
        else:
            print("  servers unpinned (the per-arm nulls are within-process drift "
                  "and do not bound cross-process placement; run --cross-null to "
                  "measure that term rather than assume it — on this workload it "
                  "measured ~1%)")
        print(f"\nsquare={SQUARE}  rounds={args.rounds}  ops/slot={args.ops}"
              f"  -P{args.pipeline}  null bound +/-{NULL_BOUND}")

        rows = []
        for label, seeds, bench_argv in shapes:
            seed(args.fr_port, seeds)
            seed(args.redis_port, seeds)
            error_probe(args.fr_port, bench_argv, "fr", label)
            error_probe(args.redis_port, bench_argv, "redis", label)
            rows.append(run_row(label, args.fr_port, args.redis_port, bench_argv,
                                args.rounds, args.ops, args.pipeline,
                                args.client_core))
            row = rows[-1]
            print(f"  {row['label']:<14} {row['ratio']:.4f}"
                  f"  [{row['ci'][0]:.4f}, {row['ci'][1]:.4f}]"
                  f"  nulls {row['null_redis']:.4f}/{row['null_fr']:.4f}"
                  f"  {row['verdict']}")

        print(f"\nRATIO = fr ops/s / redis ops/s   (>1 means FrankenRedis faster)")
        print(f"{'shape':<14}{'ratio':>9}{'95% CI':>22}"
              f"{'null redis':>12}{'null fr':>10}  verdict")
        for row in rows:
            ci_text = f"[{row['ci'][0]:.4f}, {row['ci'][1]:.4f}]"
            print(f"{row['label']:<14}{row['ratio']:>9.4f}{ci_text:>22}"
                  f"{row['null_redis']:>12.4f}{row['null_fr']:>10.4f}  {row['verdict']}")
        admissible = [r for r in rows if r["verdict"] == "ADMISSIBLE"]
        print(f"\n{len(admissible)} of {len(rows)} rows admissible; "
              f"{sum(1 for r in rows if r['verdict'] == 'NULL-FAILED')} null-failed")
        return 0
    finally:
        for proc in (fr, redis):
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
